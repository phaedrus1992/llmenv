//! The credential cache: the adapter copy of the last live credential, and the steps that seed
//! a folder from it or cache a folder credential into it.

use std::path::{Path, PathBuf};

use super::{Backend, CACHE_FILE, Credentials, dead_folder, now_ms, read_backend, write_backend};

/// Cache file path: `<state_dir>/auth/credentials.json`.
#[must_use]
pub(crate) fn cache_path(adapter_root: &Path) -> PathBuf {
    super::super::auth_cache_dir(adapter_root).join(CACHE_FILE)
}

/// Read the cached credential blob, if any.
///
/// A corrupt cache file is reported as absent (traced at debug) rather than
/// failing — a half-written blob must not wedge every future export.
///
/// # Errors
/// Returns an error only when the file exists but cannot be read.
pub(crate) fn load_cached(adapter_root: &Path) -> anyhow::Result<Option<Credentials>> {
    let path = cache_path(adapter_root);
    let bytes = match crate::paths::bounded::state_bytes_io(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => {
            let creds = Credentials::from_json(value);
            if creds.is_none() {
                report_ignored(&format!(
                    "the cached credential at {} holds no OAuth credential, so it is ignored \
                     and then replaced. Fix: run `llmenv login`.",
                    path.display()
                ));
            }
            Ok(creds)
        }
        Err(e) => {
            report_ignored(&format!(
                "the cached credential at {} is not valid JSON (line {}, column {}), so it is \
                 ignored and then replaced. Fix: run `llmenv login`.",
                path.display(),
                e.line(),
                e.column()
            ));
            Ok(None)
        }
    }
}

/// Tells the person and the agent about a credential file that is ignored. The stderr line shows
/// beside the account line, as for a skipped auth entry (#3842), and the notice reaches the
/// agent. A log line alone left the person without a reason for the replaced account (#3904).
pub(super) fn report_ignored(text: &str) {
    eprintln!("[llmenv] auth: {text}");
    crate::agent_notice::record_logged(format!("llmenv: auth: {text}"));
}

/// Write the credential cache, owner-only (0o600) and atomically.
///
/// # Errors
/// Returns an error when serialization or the atomic write fails.
pub(crate) fn save_cached(adapter_root: &Path, creds: &Credentials) -> anyhow::Result<()> {
    let path = cache_path(adapter_root);
    let blob = creds.to_blob()?;
    crate::paths::write_owner_only_atomic(&path, blob.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing credential cache {}: {e}", path.display()))?;
    // A cached credential supersedes an earlier dead read, so the next export may read again.
    dead_folder::clear(adapter_root)
}

/// Cache the folder's credential when the cache is empty or holds a dead blob.
///
/// Returns `true` when the cache was written. Never overwrites a live cached
/// blob with the folder's — the folder may be a stale rendering.
///
/// # Errors
/// Returns an error when a backend read or the cache write fails.
pub(crate) fn cache_if_needed(
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
    // A recent dead read holds the folder back, so the keychain is not read on every export (#3008).
    if dead_folder::is_recent(adapter_root, now)? {
        return Ok(false);
    }
    let Some(folder) = read_backend(backend, config_dir)? else {
        return Ok(false);
    };
    if folder.is_expired(now) {
        // A dead blob would only replace another dead blob in the cache, so it is not written.
        dead_folder::mark(adapter_root, now)?;
        return Ok(false);
    }
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
pub(crate) fn inject_if_missing(
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
        tracing::warn!(
            "cached credential has expired, so it is not injected. Fix: run `llmenv login`."
        );
        return Ok(false);
    }
    if read_backend(backend, config_dir)?.is_some() {
        return Ok(false);
    }
    write_backend(backend, config_dir, &cached)?;
    Ok(true)
}
