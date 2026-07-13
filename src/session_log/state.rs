//! Persists the `claude_session_id -> icm_session_id` correlation under the
//! stable state dir so every hook process for a launch records into the same
//! transcript session.
//!
//! The public functions resolve `state_dir()`; the `*_at` helpers take an
//! explicit path so tests exercise the on-disk format without touching the
//! global state-dir env var (mirrors `crate::icm`'s `write_memory`/`read_memory`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use llmenv_paths::{state_dir, write_owner_only_atomic};

/// Cap on the correlation map's entry count (#509 item 2). Nothing ever
/// removes an entry, so a long-lived install accumulates them forever
/// without a bound. This caps *concurrently open* sessions, not lifetime
/// session count — a session whose `SessionEnd` hasn't fired yet is still
/// evictable (see the eviction-order comment in `record_at`). 1000
/// simultaneously open sessions is unrealistic for a single install.
const MAX_CORRELATION_ENTRIES: usize = 1000;

/// Path to the correlation map file.
///
/// Falls back to a relative path in CWD when `state_dir()` cannot be resolved
/// (e.g. `$HOME` not set), so transcript correlation never silently breaks
/// even without filesystem state dir. The fallback logs a `warn!`.
#[must_use]
pub fn state_path() -> PathBuf {
    state_dir()
        .map(|d| d.join("transcript-sessions.json"))
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "cannot resolve state dir for transcript-sessions.json, \
                 using CWD fallback"
            );
            PathBuf::from("transcript-sessions.json")
        })
}

/// Load the correlation map at `path`. A missing file is the normal
/// first-run case and returns an empty map silently. A file that exists but
/// fails to parse (truncated by a crash, hand-edited, corrupted) is handled
/// fail-soft: the map degrades to empty and a `warn!` message is logged.
///
/// **Deliberate deviation from hard-error convention:** Unlike persistent
/// config (plugins, `.claude.json`), session correlation state is ephemeral
/// and low-stakes — loss of correlation just means new session IDs next
/// launch. Session logging must never block a session over a corrupt state
/// file; fail-soft is correct here. The tradeoff prioritizes availability
/// over corruption detection (contrast: #522 treats corrupt `installed_plugins.json`
/// as a hard error to avoid losing version pins).
fn load_at(path: &Path) -> BTreeMap<String, String> {
    let Ok(s) = std::fs::read_to_string(path).inspect_err(|e| {
        tracing::warn!(path = %path.display(), error = %e, "failed to read transcript-sessions.json, resetting");
    }) else {
        return BTreeMap::default();
    };
    serde_json::from_str(&s).unwrap_or_else(|e| {
        tracing::warn!(path = %path.display(), error = %e, "corrupt transcript-sessions.json, resetting");
        BTreeMap::default()
    })
}

fn record_at(path: &Path, claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut map = load_at(path);
    map.insert(claude_session_id.to_string(), icm_session_id.to_string());
    // ponytail: eviction order is BTreeMap key order (the random session-id
    // string), not recency — this format carries no timestamp to sort by.
    // Correlation entries are looked up by exact claude_session_id, and an
    // evicted-but-still-live session just starts a fresh transcript on its
    // next lookup miss (no data loss, no error). Upgrade to a timestamped
    // structure if true LRU ever matters.
    while map.len() > MAX_CORRELATION_ENTRIES {
        map.pop_first();
    }
    let body = serde_json::to_string(&map)?;
    write_owner_only_atomic(path, body.as_bytes())?;
    Ok(())
}

/// The ICM session id for a Claude session, if recorded in the map at `path`.
/// Crate-internal: lets callers that already resolved `state_path()` (e.g. one
/// hook invocation handling several events) avoid re-resolving it, and lets
/// tests exercise this without touching the global state-dir env var.
#[must_use]
pub(crate) fn lookup_session_at(path: &Path, claude_session_id: &str) -> Option<String> {
    load_at(path).get(claude_session_id).cloned()
}

/// Record the correlation into the map at `path` (read-modify-write, atomic,
/// 0o600). See [`lookup_session_at`] for why this takes an explicit path.
///
/// # Errors
/// Directory creation or atomic-write failure.
pub(crate) fn record_session_at(
    path: &Path,
    claude_session_id: &str,
    icm_session_id: &str,
) -> anyhow::Result<()> {
    record_at(path, claude_session_id, icm_session_id)
}

/// The ICM session id for a Claude session, if recorded.
#[must_use]
pub fn lookup_session(claude_session_id: &str) -> Option<String> {
    lookup_session_at(&state_path(), claude_session_id)
}

/// Record the correlation (read-modify-write, atomic, 0o600).
///
/// # Errors
/// Atomic-write failure.
pub fn record_session(claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()> {
    record_session_at(&state_path(), claude_session_id, icm_session_id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn record_then_lookup_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        assert!(load_at(&path).is_empty());
        record_at(&path, "claude-1", "icm-aaa").unwrap();
        record_at(&path, "claude-2", "icm-bbb").unwrap();
        let map = load_at(&path);
        assert_eq!(map.get("claude-1").map(String::as_str), Some("icm-aaa"));
        assert_eq!(map.get("claude-2").map(String::as_str), Some("icm-bbb"));
        assert_eq!(map.get("missing"), None);
    }

    #[test]
    fn record_at_caps_correlation_map_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        for i in 0..MAX_CORRELATION_ENTRIES + 10 {
            record_at(&path, &format!("claude-{i:05}"), "icm-x").unwrap();
        }
        let map = load_at(&path);
        assert_eq!(map.len(), MAX_CORRELATION_ENTRIES);
    }

    #[test]
    fn load_at_degrades_to_empty_on_corrupt_json_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(load_at(&path).is_empty());
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn state_file_is_owner_only(id in "[a-z0-9-]{1,16}") {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("transcript-sessions.json");
            record_at(&path, &id, "icm-x").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                prop_assert_eq!(mode & 0o077, 0);
            }
        }
    }
}
