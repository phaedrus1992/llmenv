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

/// Path to the correlation map file.
///
/// # Errors
/// Propagates `state_dir()` failure.
pub fn state_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("transcript-sessions.json"))
}

/// Load the correlation map at `path`. A missing file is the normal
/// first-run case and returns an empty map silently. A file that exists but
/// fails to parse (truncated by a crash, hand-edited, corrupted) is **not**
/// the same situation — that's data loss waiting to happen on the next write
/// (`record_at` round-trips through this), so it's logged at `warn!` even
/// though the map still degrades to empty (fail-soft: a corrupt correlation
/// file must not break session logging).
fn load_at(path: &Path) -> BTreeMap<String, String> {
    let Ok(s) = std::fs::read_to_string(path) else {
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
    let path = state_path().ok()?;
    lookup_session_at(&path, claude_session_id)
}

/// Record the correlation (read-modify-write, atomic, 0o600).
///
/// # Errors
/// Path resolution or atomic-write failure.
pub fn record_session(claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()> {
    let path = state_path()?;
    record_session_at(&path, claude_session_id, icm_session_id)
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
