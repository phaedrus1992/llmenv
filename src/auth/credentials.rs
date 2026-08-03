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

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Credential store path relative to `CLAUDE_CONFIG_DIR`.
const CREDENTIALS_FILE: &str = ".credentials.json";
/// Top-level key carrying the OAuth token set.
const OAUTH_KEY: &str = "claudeAiOauth";
/// Sibling key in the same store holding per-MCP-server OAuth tokens, keyed
/// `<server-name>|<sha256({type,url,headers})[..16]>` (#1058).
const MCP_OAUTH_KEY: &str = "mcpOAuth";
/// Cache file name under the durable auth dir.
pub(super) const CACHE_FILE: &str = "credentials.json";
/// Service-name prefix Claude Code uses for its keychain credential item.
const KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials";
/// Hex characters of the config-dir digest Claude Code appends to the service.
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
    pub const fn detect() -> Self {
        Self::Keychain
    }

    /// The backend Claude Code uses on this platform.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub const fn detect() -> Self {
        Self::File
    }
}

/// An OAuth credential blob: `{"claudeAiOauth": {accessToken, expiresAt, …}}`.
///
/// Held as opaque JSON and re-injected verbatim — llmenv never needs to read the
/// token itself, only its expiry timestamps.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials(serde_json::Value);

/// Redacting `Debug` — a derived one would print the access and refresh tokens
/// verbatim into any `{:?}` or `tracing` field. Only the expiry timestamps are
/// safe to show, and they're the only part worth debugging.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("expires_at", &self.expires_at())
            .field("refresh_expires_at", &self.refresh_expires_at())
            .field("token", &"<redacted>")
            .finish()
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
    pub fn from_json(value: serde_json::Value) -> Option<Self> {
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
    pub fn mcp_server_count(&self) -> usize {
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
    pub fn expires_at(&self) -> Option<i64> {
        self.0.get(OAUTH_KEY)?.get("expiresAt")?.as_i64()
    }

    /// Refresh-token expiry, epoch **milliseconds**.
    #[must_use]
    pub fn refresh_expires_at(&self) -> Option<i64> {
        self.0
            .get(OAUTH_KEY)?
            .get("refreshTokenExpiresAt")?
            .as_i64()
    }

    /// True when this blob is worthless: the access token has expired *and* no
    /// live refresh token remains. A stale access token with a live refresh
    /// token is still worth caching — Claude Code renews it on next use.
    ///
    /// A blob with no `expiresAt` is treated as live; an unknown expiry is not
    /// grounds for discarding a credential.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        // MCP server tokens are siblings of the login token in the same store and
        // expire independently, so a dead login token must not discard them —
        // that would silently log the user out of every third-party MCP server
        // (Slack, Notion, …). MCP entries carry no expiry of their own; Claude
        // Code refreshes or re-authorizes them per server (#1058).
        if self.mcp_server_count() > 0 {
            return false;
        }
        let access_dead = self.expires_at().is_some_and(|t| t <= now_ms);
        let refresh_live = self.refresh_expires_at().is_some_and(|t| t > now_ms);
        access_dead && !refresh_live
    }

    /// [`Credentials::is_expired`] against the current wall clock.
    #[must_use]
    pub fn is_expired_now(&self) -> bool {
        self.is_expired(now_ms())
    }

    /// The raw document, for verbatim re-injection.
    #[must_use]
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// Serialize for a backend write.
    fn to_blob(&self) -> anyhow::Result<String> {
        serde_json::to_string(&self.0)
            .map_err(|e| anyhow::anyhow!("serializing credential blob: {e}"))
    }
}

/// The macOS keychain service name Claude Code derives from a config dir:
/// `Claude Code-credentials-<sha256(path)[..8]>`.
#[must_use]
pub fn keychain_service(config_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!(
        "{KEYCHAIN_SERVICE_PREFIX}-{}",
        &digest[..SERVICE_DIGEST_LEN]
    )
}

/// Cache file path: `<state_dir>/auth/credentials.json`.
#[must_use]
pub fn cache_path(adapter_root: &Path) -> PathBuf {
    super::auth_cache_dir(adapter_root).join(CACHE_FILE)
}

/// Read the cached credential blob, if any.
///
/// A corrupt cache file is reported as absent (traced at debug) rather than
/// failing — a half-written blob must not wedge every future export.
///
/// # Errors
/// Returns an error only when the file exists but cannot be read.
pub fn load_cached(adapter_root: &Path) -> anyhow::Result<Option<Credentials>> {
    let path = cache_path(adapter_root);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => Ok(Credentials::from_json(value)),
        Err(e) => {
            tracing::debug!(
                "cached credential blob at {} is not usable (line {}, column {})",
                path.display(),
                e.line(),
                e.column()
            );
            Ok(None)
        }
    }
}

/// Write the credential cache, owner-only (0o600) and atomically.
///
/// # Errors
/// Returns an error when serialization or the atomic write fails.
pub fn save_cached(adapter_root: &Path, creds: &Credentials) -> anyhow::Result<()> {
    let path = cache_path(adapter_root);
    let blob = creds.to_blob()?;
    crate::paths::write_owner_only_atomic(&path, blob.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing credential cache {}: {e}", path.display()))
}

/// Read the credential a materialized folder currently holds.
///
/// # Errors
/// Returns an error when the backend is reachable but unreadable.
pub fn read_backend(backend: Backend, config_dir: &Path) -> anyhow::Result<Option<Credentials>> {
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
pub fn write_backend(
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
    let bytes = match std::fs::read(&path) {
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

/// Cache the folder's credential when the cache is empty or holds a dead blob.
///
/// Returns `true` when the cache was written. Never overwrites a live cached
/// blob with the folder's — the folder may be a stale rendering.
///
/// # Errors
/// Returns an error when a backend read or the cache write fails.
pub fn cache_if_needed(
    backend: Backend,
    adapter_root: &Path,
    config_dir: &Path,
) -> anyhow::Result<bool> {
    // Cache first: it is a local file read, whereas the folder's backend may be
    // the macOS keychain (a `security` subprocess). A live cached blob wins
    // regardless, so reading the folder in that case would be pure overhead on
    // a path that runs on every `export`.
    let now = now_ms();
    if load_cached(adapter_root)?.is_some_and(|c| !c.is_expired(now)) {
        return Ok(false);
    }
    let Some(folder) = read_backend(backend, config_dir)? else {
        return Ok(false);
    };
    save_cached(adapter_root, &folder)?;
    Ok(true)
}

/// Seed a materialized folder from the cache when it has no credential.
///
/// Returns `true` when a credential was injected. Never overwrites one the
/// folder already holds, and skips a cached blob that is itself dead.
///
/// # Errors
/// Returns an error when a backend read or the backend write fails.
pub fn inject_if_missing(
    backend: Backend,
    adapter_root: &Path,
    config_dir: &Path,
) -> anyhow::Result<bool> {
    // Cache first, for the same reason as `cache_if_needed`: nothing usable
    // cached means nothing to inject, so the folder's backend need not be read.
    let Some(cached) = load_cached(adapter_root)? else {
        return Ok(false);
    };
    if cached.is_expired(now_ms()) {
        tracing::debug!("cached credential has expired; not injecting (re-run `llmenv login`)");
        return Ok(false);
    }
    if read_backend(backend, config_dir)?.is_some() {
        return Ok(false);
    }
    write_backend(backend, config_dir, &cached)?;
    Ok(true)
}

/// Drop the keychain credential belonging to a config dir that no longer exists.
///
/// Returns `true` when an item was deleted. No-op off macOS, where the
/// credential lives inside the folder and dies with it.
///
/// # Errors
/// Returns an error when `security` cannot be run.
#[cfg(target_os = "macos")]
pub fn forget(config_dir: &Path) -> anyhow::Result<bool> {
    let service = keychain_service(config_dir);
    let account = keychain_account()?;
    // stderr discarded deliberately (#1092): the overwhelmingly common failure
    // is "item not found", which the `bool` return already communicates as
    // `false` — there's nothing more a caller needs from stderr for a delete.
    let status = std::process::Command::new(SECURITY_BIN)
        .args(["delete-generic-password", "-s", &service, "-a", &account])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow::anyhow!("running `security delete-generic-password`: {e}"))?;
    Ok(status.success())
}

/// Drop the keychain credential belonging to a config dir that no longer exists.
///
/// Returns `true` when an item was deleted. No-op off macOS, where the
/// credential lives inside the folder and dies with it.
///
/// # Errors
/// Never fails on this platform.
#[cfg(not(target_os = "macos"))]
pub fn forget(_config_dir: &Path) -> anyhow::Result<bool> {
    Ok(false)
}

/// Absolute path so a hijacked `PATH` cannot substitute the keychain tool.
#[cfg(target_os = "macos")]
const SECURITY_BIN: &str = "/usr/bin/security";

#[cfg(target_os = "macos")]
fn keychain_account() -> anyhow::Result<String> {
    std::env::var("USER")
        .map_err(|_| anyhow::anyhow!("USER is unset; cannot address the keychain credential item"))
}

/// `security find-generic-password`'s exit code for "no matching item"
/// (errSecItemNotFound, OSStatus -25300) — verified empirically, since
/// `security` does not document its exit codes. Classifying on this rather
/// than on stderr text matters (#1092): `security` localizes its error
/// strings via `SecCopyErrorMessageString`, so on a non-English system a
/// string match would silently miss every failure and collapse back to the
/// same "absent" outcome the classification exists to avoid.
#[cfg(target_os = "macos")]
const SECURITY_ITEM_NOT_FOUND_EXIT_CODE: i32 = 44;

/// Why `security find-generic-password` failed — distinguishing these matters
/// because "genuinely absent" and "everything else" call for different
/// user-facing responses (#1092): a real problem (locked keychain,
/// permission error, …) should surface as an error, not collapse into the
/// same "no credential stored" outcome as a missing item.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeychainReadFailure {
    /// No matching item exists — the common, expected case
    /// ([`SECURITY_ITEM_NOT_FOUND_EXIT_CODE`]).
    Absent,
    /// Any other failure: locked keychain, permission error, I/O error, etc.
    /// Treated as a real problem rather than "no credential stored".
    Other,
}

/// Classify a failed `security find-generic-password` invocation from its
/// exit status. Pure function so it's testable without a real keychain.
#[cfg(target_os = "macos")]
fn classify_keychain_read_failure(status: &std::process::ExitStatus) -> KeychainReadFailure {
    if status.code() == Some(SECURITY_ITEM_NOT_FOUND_EXIT_CODE) {
        KeychainReadFailure::Absent
    } else {
        KeychainReadFailure::Other
    }
}

#[cfg(target_os = "macos")]
fn keychain_read(config_dir: &Path) -> anyhow::Result<Option<Credentials>> {
    let service = keychain_service(config_dir);
    let account = keychain_account()?;
    let out = std::process::Command::new(SECURITY_BIN)
        .args([
            "find-generic-password",
            "-w",
            "-s",
            &service,
            "-a",
            &account,
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("running `security find-generic-password`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return match classify_keychain_read_failure(&out.status) {
            KeychainReadFailure::Absent => {
                tracing::debug!("keychain item {service} not found: {}", stderr.trim());
                Ok(None)
            }
            KeychainReadFailure::Other => Err(anyhow::anyhow!(
                "keychain lookup failed for service {service} (status {}): {}. If the \
                 keychain is locked, unlock it (e.g. `security unlock-keychain`) and retry.",
                out.status,
                stderr.trim()
            )),
        };
    }
    // Deliberately not logged or surfaced — stdout is the token itself.
    let raw = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str::<serde_json::Value>(raw.trim()) {
        Ok(value) => Ok(Credentials::from_json(value)),
        Err(e) => {
            tracing::debug!(
                "keychain item {service} is not a JSON credential (line {}, column {})",
                e.line(),
                e.column()
            );
            Ok(None)
        }
    }
}

/// Store a credential in the keychain via the `security` CLI.
///
/// The token is passed as an argv entry (`-w <blob>`), which means it is readable
/// from `ps` by other processes **of the same user** for the child's lifetime.
/// That is a known and deliberate tradeoff, not an oversight:
///
/// - Feeding the secret over stdin (bare `-w`) is not viable: `security`'s
///   interactive read truncates at 128 bytes and real credential blobs are ~510,
///   so it would silently store a corrupt token with no error. Measured, not
///   assumed.
/// - The exposure does not grant new access. Anything running as this user can
///   already read the keychain item outright, or `.credentials.json` on the file
///   backend.
///
/// Closing it properly means talking to the Security framework directly instead
/// of shelling out — tracked separately (#1061).
#[cfg(target_os = "macos")]
fn keychain_write(config_dir: &Path, creds: &Credentials) -> anyhow::Result<()> {
    let service = keychain_service(config_dir);
    let account = keychain_account()?;
    let blob = creds.to_blob()?;
    // `-U` updates in place when the item already exists. `blob` must never reach
    // a log line or error message.
    let out = std::process::Command::new(SECURITY_BIN)
        .args([
            "add-generic-password",
            "-U",
            "-s",
            &service,
            "-a",
            &account,
            "-w",
            &blob,
        ])
        .stdout(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("running `security add-generic-password`: {e}"))?;
    let stderr = redact_blob_from_stderr(&String::from_utf8_lossy(&out.stderr), &blob);
    anyhow::ensure!(
        out.status.success(),
        "`security add-generic-password` failed for service {service} (status {}): {}",
        out.status,
        stderr.trim()
    );
    Ok(())
}

/// `blob` must never reach a log line or error message (see [`keychain_write`]'s
/// doc comment): redact it from `stderr` on the off chance a future `security`
/// diagnostic echoes back its own argv.
#[cfg(target_os = "macos")]
fn redact_blob_from_stderr(stderr: &str, blob: &str) -> String {
    stderr.replace(blob, "<redacted>")
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

    // -- keychain_read failure classification (#1092) --

    #[cfg(target_os = "macos")]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classifies_item_not_found_exit_code_as_absent() {
        assert_eq!(
            classify_keychain_read_failure(&exit_status(SECURITY_ITEM_NOT_FOUND_EXIT_CODE)),
            KeychainReadFailure::Absent
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classifies_any_other_exit_code_as_other() {
        for code in [1, 36, 51, 2] {
            assert_eq!(
                classify_keychain_read_failure(&exit_status(code)),
                KeychainReadFailure::Other,
                "exit code {code} should classify as Other"
            );
        }
    }

    // -- keychain_write stderr redaction (#1092) --

    #[cfg(target_os = "macos")]
    #[test]
    fn redacts_blob_from_stderr_when_present() {
        let blob = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-TESTONLY"}}"#;
        let stderr = format!("security: usage error near argument `{blob}`\n");
        let redacted = redact_blob_from_stderr(&stderr, blob);
        assert!(!redacted.contains(blob), "blob leaked into: {redacted}");
        assert!(redacted.contains("<redacted>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn redact_blob_from_stderr_is_a_noop_when_blob_absent() {
        let stderr = "security: SecKeychainAddGenericPassword: Permission denied\n";
        assert_eq!(redact_blob_from_stderr(stderr, "some-blob"), stderr);
    }

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

    /// A blob carrying MCP server tokens is worth keeping even when the login
    /// token is dead — they authenticate different things and expire
    /// independently (#1058). Discarding it would silently log the user out of
    /// every third-party MCP server (Slack, Notion, …).
    #[test]
    fn dead_login_token_with_mcp_tokens_is_not_expired() {
        let mut doc = blob(PAST_MS, Some(PAST_MS));
        doc["mcpOAuth"] = serde_json::json!({
            "notion|0123456789abcdef": { "accessToken": "mcp-TESTONLY" }
        });
        let creds = Credentials::from_json(doc).unwrap();
        assert!(
            !creds.is_expired(PAST_MS + 1),
            "a dead login token must not discard live MCP server tokens"
        );
    }

    /// Without any MCP tokens, a dead login token is still worthless.
    #[test]
    fn dead_login_token_without_mcp_tokens_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, Some(PAST_MS))).unwrap();
        assert!(creds.is_expired(PAST_MS + 1));
    }

    /// An empty `mcpOAuth` map is not a reason to keep a dead blob.
    #[test]
    fn empty_mcp_map_does_not_rescue_a_dead_blob() {
        let mut doc = blob(PAST_MS, Some(PAST_MS));
        doc["mcpOAuth"] = serde_json::json!({});
        let creds = Credentials::from_json(doc).unwrap();
        assert!(creds.is_expired(PAST_MS + 1));
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

    #[test]
    fn live_access_token_is_not_expired() {
        let creds = Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap();
        assert!(!creds.is_expired(PAST_MS));
    }

    #[test]
    fn stale_access_token_with_live_refresh_is_not_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, Some(FUTURE_MS))).unwrap();
        assert!(!creds.is_expired(PAST_MS + 1));
    }

    #[test]
    fn stale_access_token_without_refresh_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, None)).unwrap();
        assert!(creds.is_expired(PAST_MS + 1));
    }

    #[test]
    fn stale_access_token_with_stale_refresh_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, Some(PAST_MS))).unwrap();
        assert!(creds.is_expired(PAST_MS + 1));
    }

    #[test]
    fn missing_expiry_is_treated_as_live() {
        let creds =
            Credentials::from_json(serde_json::json!({ "claudeAiOauth": { "accessToken": "x" } }))
                .unwrap();
        assert!(!creds.is_expired(FUTURE_MS));
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
    // developer's real keychain. The macOS path is exercised by `doctor --gc`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn forget_is_a_noop_where_credentials_live_in_the_folder() {
        assert!(!forget(Path::new("/nonexistent/llmenv/test/config/dir")).unwrap());
    }
}
