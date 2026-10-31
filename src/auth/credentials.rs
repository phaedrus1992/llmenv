//! Durable Claude Code OAuth credential cache (#1057).
//!
//! [`super`] caches the `oauthAccount` identity block from `.claude.json`. That
//! says *who* you are, not that you are logged in — the OAuth token itself lives
//! in a separate store keyed by `CLAUDE_CONFIG_DIR`, so it dies with every
//! content-hash change. This module caches that token in the durable state dir
//! and re-seeds a freshly materialized folder from it.
//!
//! **Backends.** On macOS the token is a keychain generic password whose service
//! name embeds a digest of `CLAUDE_CONFIG_DIR`, so it is *not* stable across
//! folders. Everywhere else (and on macOS with the keychain disabled) it is
//! `<CLAUDE_CONFIG_DIR>/.credentials.json`. [`Backend`] selects between them;
//! [`Backend::detect`] picks the platform default and tests pass
//! [`Backend::File`] explicitly so CI never needs a keychain.
//!
//! **Cache layout**: `<state_dir>/auth/credentials.json`, owner-only (0o600).
//! One file — Claude Code holds one active credential per config dir.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

// Only `keychain_service` hashes, and that is macOS/test-only — see its cfg.
#[cfg(any(target_os = "macos", test))]
use sha2::{Digest, Sha256};

mod cache;
mod dead_folder;
mod debug;
mod expiry;
mod keychain;
pub(crate) use cache::{cache_if_needed, cache_path, inject_if_missing, load_cached, save_cached};
pub(crate) use keychain::forget;
#[cfg(test)]
use keychain::parse_keychain_stdout;
#[cfg(target_os = "macos")]
use keychain::{keychain_read, keychain_write};

/// Credential store path relative to `CLAUDE_CONFIG_DIR`.
pub(crate) const CREDENTIALS_FILE: &str = ".credentials.json";
/// Top-level key carrying the OAuth token set.
const OAUTH_KEY: &str = "claudeAiOauth";
/// Sibling key in the same store holding per-MCP-server OAuth tokens, keyed
/// `<server-name>|<sha256({type,url,headers})[..16]>` (#1058).
const MCP_OAUTH_KEY: &str = "mcpOAuth";
/// Cache file name under the durable auth dir.
pub(super) const CACHE_FILE: &str = "credentials.json";
/// Service-name prefix Claude Code uses for its keychain credential item.
///
/// The keychain is macOS-only, so every non-test caller sits behind
/// `cfg(target_os = "macos")`; without the same gate this is dead code on other
/// targets, which only became visible once #1314 narrowed it from `pub`.
#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials";
/// Hex characters of the config-dir digest Claude Code appends to the service.
#[cfg(any(target_os = "macos", test))]
const SERVICE_DIGEST_LEN: usize = 8;

/// Where a materialized folder's OAuth token lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `<config_dir>/.credentials.json`.
    File,
    /// macOS keychain generic password, addressed by [`keychain_service`].
    #[cfg(target_os = "macos")]
    Keychain,
}

impl Backend {
    /// The backend Claude Code uses on this platform.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub(crate) const fn detect() -> Self {
        Self::Keychain
    }

    /// The backend Claude Code uses on this platform.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub(crate) const fn detect() -> Self {
        Self::File
    }
}

/// An OAuth credential blob: `{"claudeAiOauth": {accessToken, expiresAt, …}}`.
///
/// Held as opaque JSON and re-injected verbatim — llmenv never needs to read the
/// token itself, only its expiry timestamps.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials(serde_json::Value);

/// Scrubs the wrapped document on drop — it carries the plaintext access and
/// refresh tokens verbatim (security-audit, #1469).
impl Drop for Credentials {
    fn drop(&mut self) {
        super::zeroize_json_value(&mut self.0);
    }
}

impl Credentials {
    /// Wrap a raw credential document.
    ///
    /// Accepts one carrying the login token, MCP server tokens, or both — they
    /// are sibling keys in the same store and expire independently, so an
    /// MCP-only document is still worth caching (#1058). `None` when it has
    /// neither.
    #[must_use]
    pub(crate) fn from_json(value: serde_json::Value) -> Option<Self> {
        let has_login = value
            .get(OAUTH_KEY)
            .is_some_and(serde_json::Value::is_object);
        let has_mcp = value
            .get(MCP_OAUTH_KEY)
            .is_some_and(serde_json::Value::is_object);
        (has_login || has_mcp).then_some(Self(value))
    }

    /// How many MCP servers have a usable token cached.
    ///
    /// Skips tokenless stubs — Claude Code writes an entry with neither token
    /// while an authorization flow is in flight and clears it afterwards.
    #[must_use]
    pub(crate) fn mcp_server_count(&self) -> usize {
        self.0
            .get(MCP_OAUTH_KEY)
            .and_then(serde_json::Value::as_object)
            .map_or(0, |servers| {
                servers
                    .values()
                    .filter(|entry| {
                        ["accessToken", "refreshToken"]
                            .iter()
                            .any(|k| entry.get(k).is_some_and(|v| !v.is_null()))
                    })
                    .count()
            })
    }

    /// Access-token expiry, epoch **milliseconds**.
    #[must_use]
    pub(crate) fn expires_at(&self) -> Option<i64> {
        self.0.get(OAUTH_KEY)?.get("expiresAt")?.as_i64()
    }

    /// Refresh-token expiry, epoch **milliseconds**.
    #[must_use]
    fn refresh_expires_at(&self) -> Option<i64> {
        self.0
            .get(OAUTH_KEY)?
            .get("refreshTokenExpiresAt")?
            .as_i64()
    }

    /// The raw document, for verbatim re-injection.
    ///
    /// Only the round-trip tests read this today; kept because it is the one
    /// accessor for the inner document and removing it would leave those tests
    /// reaching into the newtype's private field instead (#1314).
    #[must_use]
    #[cfg(test)]
    fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// Serialize for a backend write. The result is scrubbed on drop — it
    /// carries the plaintext access and refresh tokens verbatim, same as
    /// `self.0` (security-audit, #1469).
    fn to_blob(&self) -> anyhow::Result<zeroize::Zeroizing<String>> {
        serde_json::to_string(&self.0)
            .map(zeroize::Zeroizing::new)
            .map_err(|e| anyhow::anyhow!("serializing credential blob: {e}"))
    }
}

/// The macOS keychain service name Claude Code derives from a config dir:
/// `Claude Code-credentials-<sha256(path)[..8]>`.
#[must_use]
#[cfg(any(target_os = "macos", test))]
fn keychain_service(config_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!(
        "{KEYCHAIN_SERVICE_PREFIX}-{}",
        &digest[..SERVICE_DIGEST_LEN]
    )
}

/// Read the credential a materialized folder currently holds.
///
/// # Errors
/// Returns an error when the backend is reachable but unreadable.
pub(crate) fn read_backend(
    backend: Backend,
    config_dir: &Path,
) -> anyhow::Result<Option<Credentials>> {
    match backend {
        Backend::File => read_credentials_file(config_dir),
        #[cfg(target_os = "macos")]
        Backend::Keychain => match keychain_read(config_dir)? {
            Some(creds) => Ok(Some(creds)),
            // macOS with the keychain disabled falls back to the file, same as
            // Linux/WSL.
            None => read_credentials_file(config_dir),
        },
    }
}

/// Write a credential into a materialized folder's backend.
///
/// # Errors
/// Returns an error when the write fails. Never includes the token value.
pub(crate) fn write_backend(
    backend: Backend,
    config_dir: &Path,
    creds: &Credentials,
) -> anyhow::Result<()> {
    match backend {
        Backend::File => {
            let path = config_dir.join(CREDENTIALS_FILE);
            let blob = creds.to_blob()?;
            crate::paths::write_owner_only_atomic(&path, blob.as_bytes())
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))
        }
        #[cfg(target_os = "macos")]
        Backend::Keychain => keychain_write(config_dir, creds),
    }
}

fn read_credentials_file(config_dir: &Path) -> anyhow::Result<Option<Credentials>> {
    let path = config_dir.join(CREDENTIALS_FILE);
    let bytes = match super::read_zeroizing(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => Ok(Credentials::from_json(value)),
        Err(e) => {
            tracing::debug!(
                "{} is not valid JSON (line {}, column {})",
                path.display(),
                e.line(),
                e.column()
            );
            Ok(None)
        }
    }
}

/// Wall clock in epoch milliseconds — the unit Claude Code's expiry fields use.
fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Comfortably in the past, so a blob using it is expired under any real clock.
    const PAST_MS: i64 = 1_000_000_000_000;
    /// Comfortably in the future (year ~2096).
    const FUTURE_MS: i64 = 4_000_000_000_000;
    fn blob(expires_at: i64, refresh_expires_at: Option<i64>) -> serde_json::Value {
        let mut oauth = serde_json::json!({
            "accessToken": "sk-ant-oat-TESTONLY",
            "refreshToken": "sk-ant-ort-TESTONLY",
            "expiresAt": expires_at,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
        });
        if let Some(refresh) = refresh_expires_at {
            oauth["refreshTokenExpiresAt"] = refresh.into();
        }
        serde_json::json!({ "claudeAiOauth": oauth })
    }

    fn write_folder_creds(config_dir: &Path, value: &serde_json::Value) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(
            config_dir.join(CREDENTIALS_FILE),
            serde_json::to_string(value).unwrap(),
        )
        .unwrap();
    }

    // -- parse_keychain_stdout --

    /// `security find-generic-password -w` appends a trailing newline;
    /// the old code stripped it with `.trim()` on an intermediate `&str`
    /// before parsing. Parsing straight from bytes must tolerate it too.
    #[test]
    fn parse_keychain_stdout_tolerates_a_trailing_newline() {
        let value =
            parse_keychain_stdout(b"{\"claudeAiOauth\":{\"accessToken\":\"x\"}}\n").unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "x");
    }

    #[test]
    fn parse_keychain_stdout_rejects_non_json() {
        assert!(parse_keychain_stdout(b"not json").is_err());
    }

    // -- keychain_service --

    #[test]
    fn keychain_service_matches_claude_codes_scheme() {
        // Regression guard on the exact derivation: prefix, separator, and the
        // 8-hex-char sha256 prefix of the config-dir path.
        assert_eq!(
            keychain_service(Path::new("/home/user/.claude")),
            "Claude Code-credentials-d1c0b541"
        );
    }

    #[test]
    fn keychain_service_differs_per_config_dir() {
        assert_ne!(
            keychain_service(Path::new("/a/.claude")),
            keychain_service(Path::new("/b/.claude"))
        );
    }

    // -- Credentials --

    #[test]
    fn from_json_rejects_document_without_oauth_key() {
        assert!(Credentials::from_json(serde_json::json!({ "other": 1 })).is_none());
    }

    #[test]
    fn from_json_rejects_non_object_oauth_value() {
        assert!(Credentials::from_json(serde_json::json!({ "claudeAiOauth": "nope" })).is_none());
    }

    #[test]
    fn expiry_fields_read_as_millis() {
        let creds = Credentials::from_json(blob(1_785_282_755_897, Some(FUTURE_MS))).unwrap();
        assert_eq!(creds.expires_at(), Some(1_785_282_755_897));
        assert_eq!(creds.refresh_expires_at(), Some(FUTURE_MS));
    }

    /// A blob with MCP tokens but no login token at all is still worth caching —
    /// `from_json` must not reject it.
    #[test]
    fn mcp_only_blob_is_accepted() {
        let doc = serde_json::json!({
            "mcpOAuth": { "slack|0123456789abcdef": { "accessToken": "mcp-TESTONLY" } }
        });
        assert!(
            Credentials::from_json(doc).is_some(),
            "an MCP-only blob must be cacheable"
        );
    }

    /// Counting is what `doctor` reports, so it must ignore tokenless stubs —
    /// Claude Code writes those transiently before a flow completes.
    #[test]
    fn mcp_server_count_skips_tokenless_stubs() {
        let mut doc = blob(FUTURE_MS, None);
        doc["mcpOAuth"] = serde_json::json!({
            "notion|aaaa": { "accessToken": "mcp-TESTONLY" },
            "slack|bbbb":  { "refreshToken": "mcp-TESTONLY" },
            "grid|cccc":   { "clientId": "stub-no-tokens" },
        });
        let creds = Credentials::from_json(doc).unwrap();
        assert_eq!(creds.mcp_server_count(), 2);
    }

    /// The whole document round-trips, so MCP tokens survive cache and inject.
    #[test]
    fn mcp_tokens_survive_a_cache_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut doc = blob(FUTURE_MS, None);
        doc["mcpOAuth"] = serde_json::json!({
            "notion|aaaa": { "accessToken": "mcp-TESTONLY" }
        });
        let creds = Credentials::from_json(doc).unwrap();
        save_cached(tmp.path(), &creds).unwrap();
        let back = load_cached(tmp.path()).unwrap().unwrap();
        assert_eq!(back.mcp_server_count(), 1);
        assert_eq!(back.as_json(), creds.as_json());
    }

    /// Security hardening (pre-pr-review finding on #1469): `to_blob` is the
    /// one place `Credentials`'s plaintext token gets serialized for a
    /// backend write — its result must be scrubbed on drop like every other
    /// credential-bearing buffer in this module, not left in a plain
    /// `String`. The type annotation only compiles once wrapped.
    #[test]
    fn to_blob_returns_a_zeroizing_string() {
        let creds = Credentials::from_json(blob(FUTURE_MS, None)).unwrap();

        let blob: zeroize::Zeroizing<String> = creds.to_blob().unwrap();

        assert!(blob.contains("sk-ant-oat-TESTONLY"));
    }

    /// Security hardening (#1469): `Credentials` holds the OAuth token
    /// verbatim in `self.0` — it must be scrubbed on drop, not left in
    /// freed-but-unscrubbed heap memory. Compiles only once `Credentials`
    /// has an explicit `impl Drop`.
    #[test]
    fn credentials_implements_drop_to_zeroize_its_token() {
        #[expect(
            drop_bounds,
            reason = "deliberate: the bound is a compile-time check that Credentials has an \
                      explicit `impl Drop`, not a (mistaken) attempt to detect droppability"
        )]
        fn assert_impls_drop<T: Drop>() {}
        assert_impls_drop::<Credentials>();
    }

    /// A derived `Debug` would print both tokens verbatim into any log line that
    /// formats a `Credentials`. Guard the redaction so it can't regress.
    #[test]
    fn debug_never_prints_the_tokens() {
        let creds = Credentials::from_json(blob(FUTURE_MS, None)).unwrap();
        let rendered = format!("{creds:?}");
        assert!(
            !rendered.contains("sk-ant-oat-TESTONLY"),
            "access token leaked into Debug: {rendered}"
        );
        assert!(
            !rendered.contains("sk-ant-ort-TESTONLY"),
            "refresh token leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("redacted"), "expected a redaction marker");
        // The expiry is the part that's actually useful to debug.
        assert!(rendered.contains(&FUTURE_MS.to_string()));
    }

    // -- cache round-trip --

    #[test]
    fn load_cached_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_cached(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn load_cached_returns_none_for_corrupt_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let path = cache_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_cached(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap();
        save_cached(tmp.path(), &creds).unwrap();
        assert_eq!(load_cached(tmp.path()).unwrap().unwrap(), creds);
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials::from_json(blob(FUTURE_MS, None)).unwrap();
        save_cached(tmp.path(), &creds).unwrap();
        let mode = std::fs::metadata(cache_path(tmp.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "cache file mode was {:o}",
            mode & 0o777
        );
    }

    // -- cache_if_needed --

    #[test]
    fn cache_if_needed_noop_when_folder_has_no_credential() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        assert!(!cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        assert!(load_cached(root.path()).unwrap().is_none());
    }

    #[test]
    fn cache_if_needed_writes_when_nothing_cached() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        write_folder_creds(folder.path(), &blob(FUTURE_MS, Some(FUTURE_MS)));
        assert!(cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        assert_eq!(
            load_cached(root.path()).unwrap().unwrap().expires_at(),
            Some(FUTURE_MS)
        );
    }

    #[test]
    fn cache_if_needed_does_not_clobber_a_live_cached_blob() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let live = Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap();
        save_cached(root.path(), &live).unwrap();
        // Folder holds a different (also live) blob; the cache must win.
        write_folder_creds(folder.path(), &blob(FUTURE_MS - 1, Some(FUTURE_MS)));
        assert!(!cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        assert_eq!(load_cached(root.path()).unwrap().unwrap(), live);
    }

    #[test]
    fn cache_if_needed_replaces_an_expired_cached_blob() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        save_cached(
            root.path(),
            &Credentials::from_json(blob(PAST_MS, Some(PAST_MS))).unwrap(),
        )
        .unwrap();
        write_folder_creds(folder.path(), &blob(FUTURE_MS, Some(FUTURE_MS)));
        assert!(cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        assert_eq!(
            load_cached(root.path()).unwrap().unwrap().expires_at(),
            Some(FUTURE_MS)
        );
    }

    // -- inject_if_missing --

    #[test]
    fn inject_if_missing_seeds_an_empty_folder() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let cached = Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap();
        save_cached(root.path(), &cached).unwrap();
        assert!(inject_if_missing(Backend::File, root.path(), folder.path()).unwrap());
        assert_eq!(
            read_backend(Backend::File, folder.path()).unwrap().unwrap(),
            cached
        );
    }

    #[test]
    fn inject_if_missing_noop_when_nothing_cached() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        assert!(!inject_if_missing(Backend::File, root.path(), folder.path()).unwrap());
        assert!(!folder.path().join(CREDENTIALS_FILE).exists());
    }

    #[test]
    fn inject_if_missing_never_overwrites_the_folders_credential() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        save_cached(
            root.path(),
            &Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap(),
        )
        .unwrap();
        let existing = blob(FUTURE_MS - 12345, Some(FUTURE_MS));
        write_folder_creds(folder.path(), &existing);
        assert!(!inject_if_missing(Backend::File, root.path(), folder.path()).unwrap());
        assert_eq!(
            read_backend(Backend::File, folder.path())
                .unwrap()
                .unwrap()
                .as_json(),
            &existing
        );
    }

    #[test]
    fn inject_if_missing_skips_an_expired_cached_blob() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        save_cached(
            root.path(),
            &Credentials::from_json(blob(PAST_MS, Some(PAST_MS))).unwrap(),
        )
        .unwrap();
        assert!(!inject_if_missing(Backend::File, root.path(), folder.path()).unwrap());
        assert!(!folder.path().join(CREDENTIALS_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn injected_folder_credential_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        save_cached(
            root.path(),
            &Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap(),
        )
        .unwrap();
        assert!(inject_if_missing(Backend::File, root.path(), folder.path()).unwrap());
        let mode = std::fs::metadata(folder.path().join(CREDENTIALS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn read_backend_ignores_a_corrupt_folder_credential() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join(CREDENTIALS_FILE), b"garbage").unwrap();
        assert!(
            read_backend(Backend::File, folder.path())
                .unwrap()
                .is_none()
        );
    }

    // Not run on macOS: there `forget` invokes `security`, which would reach the
    // developer's real keychain. The macOS path is exercised by `llmenv clean`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn forget_is_a_noop_where_credentials_live_in_the_folder() {
        assert!(!forget(Path::new("/nonexistent/llmenv/test/config/dir")).unwrap());
    }
}
