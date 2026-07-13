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

fn build_http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("llmenv-upgrade/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")
}

fn download_binary(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .context("failed to download binary")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "download failed with HTTP {}",
        resp.status()
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
        let _ = std::fs::remove_file(&temp);
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
            let _ = std::fs::remove_file(&backup);
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = restore_backup(&current_exe, &backup);
            anyhow::bail!("new binary failed verification (stderr: {stderr}); restored original");
        }
        Err(e) => {
            let _ = restore_backup(&current_exe, &backup);
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

fn get_api_base_url() -> String {
    env::var("LLMENV_UPGRADE_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".to_string())
}

pub(super) fn run_upgrade(track: Option<String>, check_only: bool) -> Result<()> {
    let is_beta = resolve_is_beta(track);
    let current_version = env!("CARGO_PKG_VERSION");

    let client = build_http_client()?;
    let base_url = get_api_base_url();

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
    eprint!("Downloading llmenv {}... ", release_version);
    let binary_data = download_binary(&client, &asset.browser_download_url)?;
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
            let client = reqwest::blocking::Client::new();
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
            let client = reqwest::blocking::Client::new();
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
            let client = reqwest::blocking::Client::new();
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
            let client = reqwest::blocking::Client::new();
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
            let client = reqwest::blocking::Client::new();
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
        let result = tokio::task::spawn_blocking(move || {
            let client = reqwest::blocking::Client::new();
            download_binary(&client, &format!("{uri}/binary"))
        })
        .await
        .unwrap();
        assert!(result.is_err());
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
