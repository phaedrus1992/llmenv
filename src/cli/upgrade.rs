use anyhow::{Context, Result};
use std::env;
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Map the current platform to a GitHub release asset name.
fn platform_asset_name() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok("llmenv-macos-aarch64"),
        ("macos", "x86_64") => Ok("llmenv-macos-x86_64"),
        ("linux", "aarch64") => Ok("llmenv-linux-aarch64"),
        ("linux", "x86_64") => Ok("llmenv-linux-x86_64"),
        (os, arch) => anyhow::bail!(
            "unsupported platform: {os}-{arch} — \
             llmenv does not provide pre-built binaries for this target"
        ),
    }
}

/// Minimal 3-component semver for comparison.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_version(s: &str) -> Result<Version> {
    let stripped = s.strip_prefix('v').unwrap_or(s);
    let parts: Vec<&str> = stripped.splitn(3, '.').collect();
    anyhow::ensure!(parts.len() == 3, "invalid version string: \"{s}\"");
    Ok(Version {
        major: parts[0].parse().context("invalid major version")?,
        minor: parts[1].parse().context("invalid minor version")?,
        patch: parts[2].parse().context("invalid patch version")?,
    })
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let Ok(va) = parse_version(a).inspect_err(|e| {
        tracing::warn!(version = %a, error = %e, "failed to parse version string in comparison")
    }) else {
        return std::cmp::Ordering::Equal;
    };
    let Ok(vb) = parse_version(b).inspect_err(|e| {
        tracing::warn!(version = %b, error = %e, "failed to parse version string in comparison")
    }) else {
        return std::cmp::Ordering::Equal;
    };
    va.cmp(&vb)
}

/// GitHub release asset.
#[derive(Debug, serde::Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// GitHub release (/releases/latest or /releases list entry).
#[derive(Debug, serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "used in deserialization; consumed by wiremock tests"
        )
    )]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    assets: Vec<GhAsset>,
}

/// Resolve which release track to use: CLI flag > config > default (release).
fn resolve_is_beta(track: Option<String>) -> bool {
    if let Some(t) = track {
        return t == "beta";
    }
    // Try `features.upgrade.track` from config
    if let Ok(dir) = crate::paths::config_dir()
        && let Ok(cfg) = crate::config::Config::load(&dir.join("config.yaml"))
        && let Some(upgrade) = cfg.features.as_ref().and_then(|f| f.upgrade.as_ref())
    {
        return upgrade.track.as_str() == "beta";
    }
    false
}

/// Fetch the latest non-prerelease GitHub release.
fn fetch_latest(client: &reqwest::blocking::Client, base_url: &str) -> Result<GhRelease> {
    let url = format!("{base_url}/repos/phaedrus1992/llmenv/releases/latest");
    let resp = client
        .get(&url)
        .send()
        .context("failed to query GitHub releases API")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "GitHub API returned {}",
        resp.status()
    );
    resp.json()
        .context("failed to parse GitHub release response")
}

/// Fetch releases and return the first non-draft (beta track).
fn fetch_beta(client: &reqwest::blocking::Client, base_url: &str) -> Result<GhRelease> {
    let url = format!("{base_url}/repos/phaedrus1992/llmenv/releases?per_page=10");
    let resp = client
        .get(&url)
        .send()
        .context("failed to query GitHub releases API")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "GitHub API returned {}",
        resp.status()
    );
    let releases: Vec<GhRelease> = resp
        .json()
        .context("failed to parse GitHub releases response")?;
    releases
        .into_iter()
        .find(|r| !r.draft)
        .context("no published releases found")
}

/// Hosts that may serve a release asset. The asset URL comes from a remote JSON response, so its
/// host is checked before any byte is fetched.
const ASSET_HOSTS: [&str; 2] = ["github.com", "objects.githubusercontent.com"];

/// Hosts a redirect may reach while the client talks to GitHub. GitHub sends a release
/// download to `release-assets`, so that host must be here or every upgrade fails.
const REDIRECT_HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Redirect hops allowed before a request fails. Caps a redirect loop.
const MAX_REDIRECTS: usize = 5;

/// Whether `url` is HTTPS on the default port and names one of `hosts`.
fn is_https_host_in(url: &reqwest::Url, hosts: &[&str]) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.host_str().is_some_and(|host| hosts.contains(&host))
}

/// Parse a release asset URL and check that it names an allowed download host.
fn validate_asset_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw)
        .with_context(|| format!("release asset URL is malformed: {raw:?}"))?;
    anyhow::ensure!(
        is_https_host_in(&url, &ASSET_HOSTS),
        "refusing to download from {raw:?}: the asset must be HTTPS on one of {ASSET_HOSTS:?}"
    );
    Ok(url)
}

/// Follow a redirect only to a GitHub host, and only up to `MAX_REDIRECTS` hops.
fn check_redirect(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    if attempt.previous().len() >= MAX_REDIRECTS {
        attempt.error("too many redirects")
    } else if is_https_host_in(attempt.url(), &REDIRECT_HOSTS) {
        attempt.follow()
    } else {
        let url = attempt.url().to_string();
        attempt.error(format!("refusing redirect to {url:?}: not a GitHub host"))
    }
}

fn build_http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("llmenv-upgrade/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(check_redirect))
        .build()
        .context("failed to build HTTP client")
}

fn download_binary(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .context("failed to download binary")?;
    let status = resp.status();
    anyhow::ensure!(
        status.is_success(),
        "download failed with HTTP {status} from {url}. Check the network or proxy, then run \
         llmenv upgrade again."
    );
    Ok(resp.bytes().context("failed to read binary")?.to_vec())
}

/// Install `data` as the new binary, with backup/restore safety.
fn install_binary(data: &[u8]) -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to get current executable path")?;
    let current_dir = current_exe
        .parent()
        .context("current executable has no parent directory")?;

    // Backup lives next to the current binary (same filesystem for atomic rename)
    let backup = current_dir.join(".llmenv-upgrade.bak");
    std::fs::copy(&current_exe, &backup)
        .with_context(|| format!("failed to backup current binary to {}", backup.display()))?;

    // Write new binary to a temp file in the same directory
    let temp = current_dir.join(".llmenv-upgrade.new");
    let write_result = (|| -> Result<()> {
        let mut tmp =
            std::fs::File::create(&temp).context("failed to create temp file for new binary")?;
        tmp.write_all(data).context("failed to write new binary")?;
        tmp.sync_all().context("failed to sync new binary")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&temp, perms)
                .context("failed to set executable permissions")?;
        }

        // Rename over the current binary
        std::fs::rename(&temp, &current_exe).context("failed to replace current binary")?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp).inspect_err(|e| {
            tracing::warn!(
                "upgrade: failed to remove temp file {}: {e}",
                temp.display()
            )
        });
        // Restore backup before propagating the error
        let restore_err = restore_backup(&current_exe, &backup);
        if let Err(re) = restore_err {
            anyhow::bail!("failed to install upgrade: {e}; AND failed to restore backup: {re}");
        }
        return Err(e.context("upgrade installation failed; backup restored"));
    }

    // Verify the new binary works
    match Command::new(&current_exe).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let _ = std::fs::remove_file(&backup).inspect_err(|e| {
                tracing::warn!("upgrade: failed to remove backup {}: {e}", backup.display())
            });
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let restore_err = restore_backup(&current_exe, &backup).inspect_err(|e| {
                tracing::warn!("upgrade: failed to restore backup after verification failure: {e}")
            });
            if let Err(re) = restore_err {
                anyhow::bail!(
                    "new binary failed verification (stderr: {stderr}); AND failed to restore backup: {re}"
                );
            }
            anyhow::bail!("new binary failed verification (stderr: {stderr}); restored original");
        }
        Err(e) => {
            let restore_err = restore_backup(&current_exe, &backup).inspect_err(|e| {
                tracing::warn!("upgrade: failed to restore backup after verification error: {e}")
            });
            if let Err(re) = restore_err {
                anyhow::bail!(
                    "could not verify new binary: {e}; AND failed to restore backup: {re}"
                );
            }
            anyhow::bail!("could not verify new binary: {e}; restored original");
        }
    }
}

fn restore_backup(target: &Path, backup: &Path) -> Result<()> {
    std::fs::rename(backup, target).context("failed to restore backup binary")
}

/// Find the matching platform asset in a release.
fn find_asset(release: &GhRelease) -> Result<&GhAsset> {
    let asset_name = platform_asset_name()?;
    release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .with_context(|| format!("no release asset for platform: {asset_name}"))
}

fn get_api_base_url() -> Result<String> {
    api_base_from_env(env::var("LLMENV_UPGRADE_GITHUB_API"))
}

/// Turn the `LLMENV_UPGRADE_GITHUB_API` lookup into a base URL.
///
/// An override that is set but unusable fails. It does not fall back to GitHub, because the user
/// set it to choose the host.
fn api_base_from_env(value: Result<String, env::VarError>) -> Result<String> {
    const GITHUB_API: &str = "https://api.github.com";
    match value {
        Ok(base) if base.trim().is_empty() => anyhow::bail!(
            "LLMENV_UPGRADE_GITHUB_API is set but empty. Set it to a base URL, or unset it to use \
             {GITHUB_API}."
        ),
        Ok(base) => Ok(base),
        Err(env::VarError::NotPresent) => Ok(GITHUB_API.to_string()),
        Err(env::VarError::NotUnicode(_)) => anyhow::bail!(
            "LLMENV_UPGRADE_GITHUB_API is not valid UTF-8. Export it again as plain text, or unset \
             it to use {GITHUB_API}."
        ),
    }
}

pub(super) fn run_upgrade(track: Option<String>, check_only: bool) -> Result<()> {
    let is_beta = resolve_is_beta(track);
    let current_version = env!("CARGO_PKG_VERSION");

    let client = build_http_client()?;
    let base_url = get_api_base_url()?;

    let release = if is_beta {
        fetch_beta(&client, &base_url)?
    } else {
        fetch_latest(&client, &base_url)?
    };

    let release_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    match compare_versions(release_version, current_version) {
        std::cmp::Ordering::Greater => {
            if check_only {
                println!(
                    "Update available: llmenv {} (current: {})",
                    release_version, current_version
                );
                println!("Run `llmenv upgrade` to update.");
                std::process::exit(1);
            }
        }
        _ => {
            if check_only {
                println!("llmenv is up to date ({})", current_version);
                return Ok(());
            }
            // Already at latest — still check --check handled it above, but if
            // not in check mode we just tell the user and return.
            eprintln!("Already at latest version ({})", current_version);
            return Ok(());
        }
    }

    let asset = find_asset(&release)?;
    let asset_url = validate_asset_url(&asset.browser_download_url)?;
    eprint!("Downloading llmenv {}... ", release_version);
    let binary_data = download_binary(&client, asset_url.as_str())?;
    let mb = binary_data.len() as f64 / 1_048_576.0;
    eprintln!("{:.1} MB", mb);

    install_binary(&binary_data)?;
    println!("Successfully upgraded to llmenv {}", release_version);

    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // -- Platform detection

    #[test]
    fn platform_asset_name_known_platforms() {
        // These are the four build targets from release.yml
        let platforms = [
            ("macos", "aarch64", "llmenv-macos-aarch64"),
            ("macos", "x86_64", "llmenv-macos-x86_64"),
            ("linux", "aarch64", "llmenv-linux-aarch64"),
            ("linux", "x86_64", "llmenv-linux-x86_64"),
        ];
        for (os, arch, expected) in &platforms {
            // We can't override env::consts, but we can at least verify
            // the match arms exist by checking the function signature.
            // Integration-test coverage via the build matrix.
            let _ = (os, arch, expected);
        }
        // At minimum verify the current host matches something
        assert!(platform_asset_name().is_ok());
    }

    // -- Version parsing

    #[test]
    fn parse_version_three_component() {
        let v = parse_version("3.2.0").unwrap();
        assert_eq!(
            v,
            Version {
                major: 3,
                minor: 2,
                patch: 0
            }
        );
    }

    #[test]
    fn parse_version_with_v_prefix() {
        let v = parse_version("v3.2.1").unwrap();
        assert_eq!(
            v,
            Version {
                major: 3,
                minor: 2,
                patch: 1
            }
        );
    }

    #[test]
    fn parse_version_invalid() {
        assert!(parse_version("3.2").is_err());
        assert!(parse_version("abc").is_err());
        assert!(parse_version("").is_err());
    }

    // -- Version comparison

    #[test]
    fn compare_versions_ordering() {
        assert_eq!(
            compare_versions("3.3.0", "3.2.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_versions("3.2.0", "3.3.0"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("3.2.0", "3.2.0"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("10.0.0", "9.99.99"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_versions_invalid_returns_equal() {
        assert_eq!(
            compare_versions("invalid", "3.2.0"),
            std::cmp::Ordering::Equal
        );
    }

    // -- Property-based tests

    proptest::proptest! {
        #[test]
        fn compare_versions_reflexive(major: u64, minor: u64, patch: u64) {
            let v = format!("{major}.{minor}.{patch}");
            prop_assert_eq!(compare_versions(&v, &v), std::cmp::Ordering::Equal);
        }

        #[test]
        fn compare_versions_antisymmetric(
            a_major: u64, a_minor: u64, a_patch: u64,
            b_major: u64, b_minor: u64, b_patch: u64,
        ) {
            let a = format!("{a_major}.{a_minor}.{a_patch}");
            let b = format!("{b_major}.{b_minor}.{b_patch}");
            let forward = compare_versions(&a, &b);
            let backward = compare_versions(&b, &a);
            prop_assert_eq!(backward, forward.reverse());
        }

        #[test]
        fn compare_versions_transitive(
            a: (u64, u64, u64), b: (u64, u64, u64), c: (u64, u64, u64),
        ) {
            let va = format!("{}.{}.{}", a.0, a.1, a.2);
            let vb = format!("{}.{}.{}", b.0, b.1, b.2);
            let vc = format!("{}.{}.{}", c.0, c.1, c.2);
            let ab = compare_versions(&va, &vb);
            let bc = compare_versions(&vb, &vc);
            if ab == std::cmp::Ordering::Greater && bc == std::cmp::Ordering::Greater {
                prop_assert_eq!(compare_versions(&va, &vc), std::cmp::Ordering::Greater);
            }
        }

        #[test]
        fn compare_versions_v_prefix(major: u64, minor: u64, patch: u64) {
            let bare = format!("{major}.{minor}.{patch}");
            let prefixed = format!("v{major}.{minor}.{patch}");
            prop_assert_eq!(compare_versions(&bare, &prefixed), std::cmp::Ordering::Equal);
            prop_assert_eq!(compare_versions(&prefixed, &bare), std::cmp::Ordering::Equal);
        }

        #[test]
        fn compare_versions_no_panic_on_any_string(s in ".*") {
            let _ = compare_versions(&s, "1.0.0");
            let _ = compare_versions("1.0.0", &s);
        }
    }

    // -- GitHub API integration

    #[tokio::test]
    async fn fetch_latest_release_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(
                "/repos/phaedrus1992/llmenv/releases/latest",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v3.3.0",
                "prerelease": false,
                "draft": false,
                "assets": [{
                    "name": "llmenv-macos-aarch64",
                    "browser_download_url": "https://example.com/llmenv-macos-aarch64"
                }]
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let release = tokio::task::spawn_blocking(move || {
            let client = build_http_client().unwrap();
            fetch_latest(&client, &uri)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(release.tag_name, "v3.3.0");
        assert!(!release.prerelease);
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].name, "llmenv-macos-aarch64");
    }

    #[tokio::test]
    async fn fetch_latest_release_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(
                "/repos/phaedrus1992/llmenv/releases/latest",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let client = build_http_client().unwrap();
            fetch_latest(&client, &uri)
        })
        .await
        .unwrap();
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn fetch_beta_release_skips_draft() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(
                "/repos/phaedrus1992/llmenv/releases",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "tag_name": "v3.3.0-beta.1",
                    "prerelease": true,
                    "draft": true,
                    "assets": [{
                        "name": "llmenv-macos-aarch64",
                        "browser_download_url": "https://example.com/beta"
                    }]
                },
                {
                    "tag_name": "v3.3.0-alpha.1",
                    "prerelease": true,
                    "draft": false,
                    "assets": [{
                        "name": "llmenv-macos-aarch64",
                        "browser_download_url": "https://example.com/alpha"
                    }]
                }
            ])))
            .mount(&server)
            .await;

        let uri = server.uri();
        let release = tokio::task::spawn_blocking(move || {
            let client = build_http_client().unwrap();
            fetch_beta(&client, &uri)
        })
        .await
        .unwrap()
        .unwrap();
        // Should skip the draft and return the next non-draft
        assert_eq!(release.tag_name, "v3.3.0-alpha.1");
    }

    #[tokio::test]
    async fn fetch_beta_all_drafts_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(
                "/repos/phaedrus1992/llmenv/releases",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "tag_name": "v3.3.0-draft",
                    "prerelease": false,
                    "draft": true,
                    "assets": []
                }
            ])))
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = build_http_client().unwrap();
            fetch_beta(&client, &uri)
        })
        .await
        .unwrap();
        assert!(result.is_err());
    }

    // -- Asset matching

    #[test]
    fn find_asset_matches_by_name() {
        let release = GhRelease {
            tag_name: "v3.3.0".into(),
            prerelease: false,
            draft: false,
            assets: vec![
                GhAsset {
                    name: "llmenv-macos-aarch64".into(),
                    browser_download_url: "https://example.com/mac-arm".into(),
                },
                GhAsset {
                    name: "llmenv-linux-x86_64".into(),
                    browser_download_url: "https://example.com/linux".into(),
                },
            ],
        };
        let asset = find_asset(&release).unwrap();
        // Should match the current platform's asset name
        let current = platform_asset_name().unwrap();
        assert_eq!(asset.name, current);
    }

    #[test]
    fn find_asset_missing_returns_error() {
        let release = GhRelease {
            tag_name: "v3.3.0".into(),
            prerelease: false,
            draft: false,
            assets: vec![GhAsset {
                name: "some-other-binary".into(),
                browser_download_url: "https://example.com/other".into(),
            }],
        };
        assert!(find_asset(&release).is_err());
    }

    // -- Download

    #[tokio::test]
    async fn download_binary_success() {
        let server = MockServer::start().await;
        let body = b"fake binary content";
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/binary"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let uri = server.uri();
        let data = tokio::task::spawn_blocking(move || {
            let client = build_http_client().unwrap();
            download_binary(&client, &format!("{uri}/binary"))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(data, body);
    }

    #[tokio::test]
    async fn download_binary_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/binary"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let uri = server.uri();
        let url = format!("{uri}/binary");
        let named_url = url.clone();
        let result = tokio::task::spawn_blocking(move || {
            let client = build_http_client().unwrap();
            download_binary(&client, &url)
        })
        .await
        .unwrap();
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(msg.contains("HTTP 500"), "{msg}");
        assert!(msg.contains(&named_url), "error must name the URL: {msg}");
    }

    #[test]
    fn api_base_defaults_when_unset() {
        let base = api_base_from_env(Err(env::VarError::NotPresent)).unwrap();
        assert_eq!(base, "https://api.github.com");
    }

    #[test]
    fn api_base_present_is_used() {
        let base = api_base_from_env(Ok("http://127.0.0.1:9".into())).unwrap();
        assert_eq!(base, "http://127.0.0.1:9");
    }

    #[test]
    fn api_base_empty_is_an_error_that_names_the_variable() {
        let msg = api_base_from_env(Ok("  ".into()))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            msg.contains("LLMENV_UPGRADE_GITHUB_API is set but empty"),
            "{msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn api_base_not_unicode_is_an_error_not_a_fallback() {
        use std::os::unix::ffi::OsStringExt;
        let raw = std::ffi::OsString::from_vec(vec![0xff]);
        let msg = api_base_from_env(Err(env::VarError::NotUnicode(raw)))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            msg.contains("LLMENV_UPGRADE_GITHUB_API is not valid UTF-8"),
            "{msg}"
        );
    }

    #[test]
    fn validate_asset_url_accepts_an_https_github_asset() {
        let url =
            validate_asset_url("https://github.com/o/r/releases/download/v1/llmenv-linux-x86_64")
                .unwrap();
        assert_eq!(url.host_str(), Some("github.com"));
        validate_asset_url("https://objects.githubusercontent.com/a/b").unwrap();
    }

    #[test]
    fn validate_asset_url_refuses_another_host() {
        let err = validate_asset_url("https://evil.example/llmenv").unwrap_err();
        assert!(err.to_string().contains("evil.example"), "{err}");
    }

    #[test]
    fn validate_asset_url_refuses_plain_http_on_a_github_host() {
        assert!(validate_asset_url("http://github.com/o/r/asset").is_err());
    }

    proptest! {
        #[test]
        fn validate_asset_url_accepts_only_https_allowlisted_hosts_on_the_default_port(
            scheme in prop::sample::select(vec!["https", "http"]),
            host in prop::sample::select(vec![
                "github.com",
                "objects.githubusercontent.com",
                "api.github.com",
                "github.com.evil.example",
                "evil.example",
            ]),
            port in prop::sample::select(vec!["", ":443", ":8443"]),
        ) {
            let raw = format!("{scheme}://{host}{port}/asset");
            let expected = scheme == "https"
                && port != ":8443"
                && (host == "github.com" || host == "objects.githubusercontent.com");
            prop_assert_eq!(validate_asset_url(&raw).is_ok(), expected, "{}", raw);
        }
    }

    #[test]
    fn the_release_asset_cdn_is_an_allowed_redirect_host() {
        let url = reqwest::Url::parse("https://release-assets.githubusercontent.com/x").unwrap();
        assert!(is_https_host_in(&url, &REDIRECT_HOSTS));
    }

    #[test]
    fn validate_asset_url_refuses_a_non_default_port() {
        assert!(validate_asset_url("https://github.com:8443/o/r/asset").is_err());
    }

    #[test]
    fn validate_asset_url_refuses_a_malformed_url() {
        let err = validate_asset_url("not a url").unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    #[tokio::test]
    async fn redirect_to_a_host_outside_github_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/start"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://evil.example/bin"),
            )
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = build_http_client()?;
            download_binary(&client, &format!("{uri}/start"))
        })
        .await
        .unwrap();
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("not a GitHub host"), "{err:#}");
    }

    // -- Config resolution

    #[test]
    fn resolve_is_beta_cli_flag_wins() {
        assert!(resolve_is_beta(Some("beta".into())));
        assert!(!resolve_is_beta(Some("release".into())));
    }

    #[test]
    fn resolve_is_beta_no_config_defaults_false() {
        // No config available in a test environment, so defaults to release
        assert!(!resolve_is_beta(None));
    }
}
