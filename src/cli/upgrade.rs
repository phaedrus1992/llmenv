use anyhow::{Context, Result};
use std::env;
use std::io::{Read, Write};
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
        // TLS to github.com is the root of trust for the whole upgrade path — it
        // is what makes the release metadata, and therefore the checksum, worth
        // anything. reqwest follows up to 10 redirects and permits an
        // https -> http downgrade by default, which would let anyone on a
        // downgraded hop serve a matched binary and checksum pair.
        .https_only(true)
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

/// Release asset holding one `sha256sum`-format line per binary, produced by
/// the release workflow's checksum step.
const CHECKSUMS_ASSET: &str = "checksums.txt";

/// Ceiling on the `checksums.txt` read. One `sha256sum` line per released asset
/// is a few hundred bytes; 64 KiB leaves room for many more platforms while
/// still bounding what a hostile endpoint can make llmenv allocate.
const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;

/// The GitHub API llmenv upgrades from.
///
/// A constant, not an override. This used to be settable via
/// `LLMENV_UPGRADE_GITHUB_API`, which nothing read — the tests pass `base_url`
/// explicitly and it was documented nowhere — but which anyone able to set an
/// environment variable could use to redirect the release lookup. Both the
/// binary URL and the `checksums.txt` URL come from that one response, so a
/// redirected lookup serves a matched pair and checksum verification proves
/// nothing. An unused env var that voids the auto-updater's trust anchor is pure
/// attack surface (#1040).
const GITHUB_API_BASE: &str = "https://api.github.com";

/// The expected SHA-256 for `asset_name`, parsed out of a `sha256sum`-format
/// listing (`<64 hex chars>  <filename>` per line, two spaces for binary mode).
///
/// Tolerant about the separator (`sha256sum` writes two spaces, ` *name` in
/// binary mode) and about the hash's case, but not about its shape: a line whose
/// first field isn't 64 hex characters is not a checksum and is ignored.
///
/// # Errors
/// Returns an error when the listing has no entry for `asset_name`, or more than
/// one that disagree. Disagreeing duplicates are a hard failure rather than a
/// first-one-wins pick: this file arrives over the network, and quietly choosing
/// between two contradictory claims about the same file is exactly the ambiguity
/// verification exists to remove.
fn expected_sha256_for(checksums: &str, asset_name: &str) -> Result<String> {
    // An empty name would otherwise match a line with an empty filename field.
    // Unreachable from `platform_asset_name`, which returns a fixed non-empty
    // string — but the parser shouldn't rely on its caller for that.
    anyhow::ensure!(
        !asset_name.is_empty(),
        "cannot look up a checksum for an empty asset name"
    );

    let mut matches = checksums
        .lines()
        .filter_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            if name.trim().trim_start_matches('*') != asset_name {
                return None;
            }
            let hash = hash.trim();
            (hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| hash.to_ascii_lowercase())
        })
        .collect::<Vec<_>>();
    matches.dedup();

    match matches.len() {
        1 => Ok(matches.swap_remove(0)),
        0 => anyhow::bail!("no checksum entry for `{asset_name}`"),
        n => anyhow::bail!("{n} conflicting checksum entries for `{asset_name}`"),
    }
}

/// Verify `data` hashes to `expected`.
///
/// # Errors
/// Returns an error naming both hashes when they differ.
fn verify_sha256(data: &[u8], expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let actual = hex::encode(Sha256::digest(data));
    anyhow::ensure!(
        actual == expected.to_ascii_lowercase(),
        "checksum mismatch: expected {expected}, got {actual}. \
         The download does not match the checksum published with the release; \
         refusing to install it."
    );
    Ok(())
}

/// Download the release's `checksums.txt` and return the expected hash for
/// `asset_name`.
///
/// Fails closed at every step (asset absent, download failed, no line for this
/// platform). Every release the upgrade path can move *to* publishes this file —
/// it has been in the release workflow since well before the oldest release
/// llmenv would upgrade from — so a missing one means the release is malformed
/// or the response isn't what it claims to be, and installing anyway would make
/// the check decorative.
fn fetch_expected_sha256(
    client: &reqwest::blocking::Client,
    release: &GhRelease,
    asset_name: &str,
) -> Result<String> {
    let checksums_asset = release
        .assets
        .iter()
        .find(|a| a.name == CHECKSUMS_ASSET)
        .with_context(|| {
            format!(
                "release has no `{CHECKSUMS_ASSET}` asset; refusing to install an unverified binary"
            )
        })?;

    let resp = client
        .get(&checksums_asset.browser_download_url)
        .send()
        .with_context(|| format!("failed to download `{CHECKSUMS_ASSET}`"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "downloading `{CHECKSUMS_ASSET}` failed with HTTP {}",
        resp.status()
    );
    // Capped: the real file is a few hundred bytes (one line per asset), and
    // reading a body of unbounded length from the network into memory to look up
    // 64 characters is a needless way to be OOM-killed.
    if let Some(len) = resp.content_length() {
        anyhow::ensure!(
            len <= MAX_CHECKSUMS_BYTES,
            "`{CHECKSUMS_ASSET}` is {len} bytes, over the {MAX_CHECKSUMS_BYTES}-byte limit"
        );
    }
    let mut body = String::new();
    resp.take(MAX_CHECKSUMS_BYTES)
        .read_to_string(&mut body)
        .with_context(|| format!("failed to read `{CHECKSUMS_ASSET}`"))?;

    expected_sha256_for(&body, asset_name).with_context(|| {
        format!("cannot verify `{asset_name}` against `{CHECKSUMS_ASSET}`; refusing to install an unverified binary")
    })
}

pub(super) fn run_upgrade(track: Option<String>, check_only: bool) -> Result<()> {
    let is_beta = resolve_is_beta(track);
    let current_version = env!("CARGO_PKG_VERSION");

    let client = build_http_client()?;
    let base_url = GITHUB_API_BASE;

    let release = if is_beta {
        fetch_beta(&client, base_url)?
    } else {
        fetch_latest(&client, base_url)?
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
    // Resolved before the download so a release without usable checksums fails
    // before spending the transfer, and so there is no path where a downloaded
    // binary exists with nothing to check it against.
    let expected_sha256 = fetch_expected_sha256(&client, &release, &asset.name)?;

    eprint!("Downloading llmenv {}... ", release_version);
    let binary_data = download_binary(&client, &asset.browser_download_url)?;
    let mb = binary_data.len() as f64 / 1_048_576.0;
    eprintln!("{:.1} MB", mb);

    verify_sha256(&binary_data, &expected_sha256)?;

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

    // -- Checksum verification (#1040)

    /// The exact format the release workflow's `sha256sum` step produces.
    const SAMPLE_CHECKSUMS: &str = "\
f43d876bddddb89cb8423278203967e41be62153c1ec562009c0cc293e185d9c  llmenv-linux-aarch64
e4b16291a02ff3029b6250758a0e0a1141d4dec181e5f619f10662da1520234d  llmenv-linux-x86_64
fda0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d  llmenv-macos-aarch64
d014cffae3326ad537b149d025f0b9c3826a91694b1c8b5717fb1f7cc8c5eea8  llmenv-macos-x86_64
";

    #[test]
    fn expected_sha256_picks_the_line_for_this_platform() {
        assert_eq!(
            expected_sha256_for(SAMPLE_CHECKSUMS, "llmenv-macos-aarch64").unwrap(),
            "fda0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d"
        );
        assert_eq!(
            expected_sha256_for(SAMPLE_CHECKSUMS, "llmenv-linux-x86_64").unwrap(),
            "e4b16291a02ff3029b6250758a0e0a1141d4dec181e5f619f10662da1520234d"
        );
    }

    /// A name that isn't listed must fail, so the caller can't install an
    /// unverified binary. Also guards against a prefix/substring match:
    /// `llmenv-macos` is not `llmenv-macos-aarch64`.
    #[test]
    fn expected_sha256_errors_for_an_unlisted_asset() {
        for name in ["llmenv-freebsd-x86_64", "llmenv-macos"] {
            let err = expected_sha256_for(SAMPLE_CHECKSUMS, name).unwrap_err();
            assert!(
                format!("{err:#}").contains("no checksum entry"),
                "for {name}: {err:#}"
            );
        }
        assert!(expected_sha256_for("", "llmenv-macos-aarch64").is_err());
    }

    /// An empty name would otherwise match a line whose filename field is empty.
    /// Unreachable from `platform_asset_name`, but the parser owns the guard.
    #[test]
    fn expected_sha256_rejects_an_empty_asset_name() {
        let hash = "a".repeat(64);
        let err = expected_sha256_for(&format!("{hash}  \n"), "").unwrap_err();
        assert!(format!("{err:#}").contains("empty asset name"), "{err:#}");
    }

    /// Two entries disagreeing about the same file mean the listing is malformed
    /// or tampered with. Picking one silently would be choosing which claim to
    /// believe — the thing verification exists to avoid.
    #[test]
    fn expected_sha256_errors_on_conflicting_duplicate_entries() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let text = format!("{a}  llmenv-macos-aarch64\n{b}  llmenv-macos-aarch64\n");
        let err = expected_sha256_for(&text, "llmenv-macos-aarch64").unwrap_err();
        assert!(
            format!("{err:#}").contains("conflicting checksum entries"),
            "{err:#}"
        );
    }

    /// A repeated *identical* line is not a conflict — nothing to choose between.
    #[test]
    fn expected_sha256_accepts_an_exactly_repeated_entry() {
        let a = "a".repeat(64);
        let text = format!("{a}  llmenv-macos-aarch64\n{a}  llmenv-macos-aarch64\n");
        assert_eq!(
            expected_sha256_for(&text, "llmenv-macos-aarch64").unwrap(),
            a
        );
    }

    /// `sha256sum` writes ` *name` in binary mode, and some tools emit a single
    /// space, uppercase hex, or CRLF line endings. All are the same statement
    /// about the same file.
    #[test]
    fn expected_sha256_tolerates_format_variants() {
        let binary_mode = "fda0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d *llmenv-macos-aarch64";
        let upper = "FDA0D9803CC9A10F82BAA07DC77ACFC6C25A8B715B339660B1A45381D739F31D  llmenv-macos-aarch64";
        let crlf = "fda0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d  llmenv-macos-aarch64\r\n";
        let tab = "fda0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d\tllmenv-macos-aarch64";
        for text in [binary_mode, upper, crlf, tab] {
            assert_eq!(
                expected_sha256_for(text, "llmenv-macos-aarch64").unwrap(),
                "fda0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d",
                "failed on {text:?}"
            );
        }
    }

    /// A malformed first field is not a checksum, so the line is ignored rather
    /// than trusted — otherwise a truncated or garbage value would become an
    /// "expected hash" that nothing could match, or a short prefix that could.
    /// A later well-formed line for the same name still wins.
    #[test]
    fn expected_sha256_ignores_lines_whose_hash_is_not_64_hex_chars() {
        for bad in [
            "deadbeef  llmenv-macos-aarch64",
            "not-a-hash-at-all  llmenv-macos-aarch64",
            "zzza0d9803cc9a10f82baa07dc77acfc6c25a8b715b339660b1a45381d739f31d  llmenv-macos-aarch64",
        ] {
            assert!(
                expected_sha256_for(bad, "llmenv-macos-aarch64").is_err(),
                "should have rejected {bad:?}"
            );
        }

        let good = "d".repeat(64);
        let mixed = format!("deadbeef  llmenv-macos-aarch64\n{good}  llmenv-macos-aarch64\n");
        assert_eq!(
            expected_sha256_for(&mixed, "llmenv-macos-aarch64").unwrap(),
            good,
            "a malformed line must not shadow a valid entry"
        );
    }

    #[test]
    fn verify_sha256_accepts_the_matching_digest() {
        // sha256("llmenv")
        let data = b"llmenv";
        let expected = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(data));
        assert!(verify_sha256(data, &expected).is_ok());
        assert!(
            verify_sha256(data, &expected.to_ascii_uppercase()).is_ok(),
            "hash comparison must not be case-sensitive"
        );
    }

    /// The point of the whole feature: bytes that don't match the published
    /// checksum are refused, and the error says both hashes.
    #[test]
    fn verify_sha256_rejects_tampered_bytes() {
        let expected = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"llmenv"));
        let err = verify_sha256(b"llmenv-but-tampered", &expected).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("checksum mismatch"), "got: {msg}");
        assert!(
            msg.contains(&expected),
            "error should name the expected hash: {msg}"
        );
    }

    proptest! {
        /// Never panics, and never invents a hash, whatever text arrives — the
        /// file is fetched over the network, so it is untrusted input.
        #[test]
        fn expected_sha256_never_panics_on_arbitrary_input(
            text in ".*",
            name in ".*",
        ) {
            if let Ok(h) = expected_sha256_for(&text, &name) {
                prop_assert_eq!(h.len(), 64);
                prop_assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
                prop_assert!(!name.is_empty(), "an empty name must never resolve");
            }
        }

        /// The positive counterpart to the property above, which only says the
        /// parser never invents a hash: a real entry, rendered the way
        /// `sha256sum` renders it, parses back to exactly that hash. Catches a
        /// parser that split the fields wrongly or mangled the hash for some
        /// name shape the example tests don't happen to use.
        ///
        /// Names are drawn from the alphabet release assets actually use. A
        /// wider generator immediately finds that a name containing *any*
        /// Unicode whitespace — a vertical tab, say — doesn't round-trip, since
        /// `split_once(char::is_whitespace)` and `str::trim` are both
        /// Unicode-aware and would treat it as a field separator. That's correct
        /// behavior for a `sha256sum` listing (such a "name" isn't one field),
        /// not a defect, so the property states what's true rather than being
        /// weakened to accommodate it.
        #[test]
        fn expected_sha256_round_trips_a_rendered_entry(
            hash in "[a-fA-F0-9]{64}",
            name in "[A-Za-z0-9][A-Za-z0-9._-]{0,40}",
        ) {
            let rendered = format!("{hash}  {name}");
            prop_assert_eq!(
                expected_sha256_for(&rendered, &name).ok(),
                Some(hash.to_ascii_lowercase())
            );
        }

        /// Unrelated entries never change the answer for a name — the lookup is
        /// keyed on the name, not on position in the file.
        #[test]
        fn unrelated_entries_do_not_affect_the_lookup(
            hash in "[a-f0-9]{64}",
            name in "[A-Za-z0-9][A-Za-z0-9._-]{0,40}",
            noise in proptest::collection::vec("[a-f0-9]{64}", 0..4),
        ) {
            let mut lines: Vec<String> = noise
                .iter()
                .enumerate()
                // Names that cannot collide with `name`, which has no space.
                .map(|(i, h)| format!("{h}  unrelated asset {i}"))
                .collect();
            lines.push(format!("{hash}  {name}"));
            prop_assert_eq!(
                expected_sha256_for(&lines.join("\n"), &name).ok(),
                Some(hash)
            );
        }
    }

    #[tokio::test]
    async fn fetch_expected_sha256_errors_when_the_release_has_no_checksums_asset() {
        let release = GhRelease {
            tag_name: "v9.9.9".into(),
            prerelease: false,
            draft: false,
            assets: vec![GhAsset {
                name: "llmenv-macos-aarch64".into(),
                browser_download_url: "https://example.com/llmenv-macos-aarch64".into(),
            }],
        };
        let err = tokio::task::spawn_blocking(move || {
            let client = reqwest::blocking::Client::new();
            fetch_expected_sha256(&client, &release, "llmenv-macos-aarch64")
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no `checksums.txt` asset"),
            "got: {err:#}"
        );
    }

    /// A release that publishes checksums but omits this platform's line is a
    /// hard failure too — the alternative is skipping verification exactly when
    /// there is nothing to verify against.
    #[tokio::test]
    async fn fetch_expected_sha256_errors_when_the_platform_is_not_listed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_CHECKSUMS))
            .mount(&server)
            .await;

        let release = GhRelease {
            tag_name: "v9.9.9".into(),
            prerelease: false,
            draft: false,
            assets: vec![GhAsset {
                name: CHECKSUMS_ASSET.into(),
                browser_download_url: format!("{}/checksums.txt", server.uri()),
            }],
        };
        let err = tokio::task::spawn_blocking(move || {
            let client = reqwest::blocking::Client::new();
            fetch_expected_sha256(&client, &release, "llmenv-freebsd-x86_64")
        })
        .await
        .unwrap()
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no checksum entry for `llmenv-freebsd-x86_64`"),
            "got: {msg}"
        );
        assert!(
            msg.contains("refusing to install an unverified binary"),
            "the caller's context must survive: {msg}"
        );
    }

    /// A hostile endpoint must not be able to make llmenv allocate an unbounded
    /// body while looking up 64 characters. The declared length is refused
    /// outright; a server that lies about it is bounded by the capped read,
    /// which then can't produce a valid entry.
    #[tokio::test]
    async fn fetch_expected_sha256_refuses_an_oversized_checksums_file() {
        let server = MockServer::start().await;
        let huge = "x".repeat(usize::try_from(MAX_CHECKSUMS_BYTES).unwrap() + 1);
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(huge))
            .mount(&server)
            .await;

        let release = GhRelease {
            tag_name: "v9.9.9".into(),
            prerelease: false,
            draft: false,
            assets: vec![GhAsset {
                name: CHECKSUMS_ASSET.into(),
                browser_download_url: format!("{}/checksums.txt", server.uri()),
            }],
        };
        let err = tokio::task::spawn_blocking(move || {
            let client = reqwest::blocking::Client::new();
            fetch_expected_sha256(&client, &release, "llmenv-macos-aarch64")
        })
        .await
        .unwrap()
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("over the") || msg.contains("no checksum entry"),
            "oversized body should have been refused or truncated to nothing: {msg}"
        );
    }

    #[tokio::test]
    async fn fetch_expected_sha256_returns_the_published_hash() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_CHECKSUMS))
            .mount(&server)
            .await;

        let release = GhRelease {
            tag_name: "v9.9.9".into(),
            prerelease: false,
            draft: false,
            assets: vec![GhAsset {
                name: CHECKSUMS_ASSET.into(),
                browser_download_url: format!("{}/checksums.txt", server.uri()),
            }],
        };
        let hash = tokio::task::spawn_blocking(move || {
            let client = reqwest::blocking::Client::new();
            fetch_expected_sha256(&client, &release, "llmenv-linux-aarch64")
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            hash,
            "f43d876bddddb89cb8423278203967e41be62153c1ec562009c0cc293e185d9c"
        );
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
