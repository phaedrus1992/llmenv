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

fn load_at(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
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

/// The ICM session id for a Claude session, if recorded.
#[must_use]
pub fn lookup_session(claude_session_id: &str) -> Option<String> {
    let path = state_path().ok()?;
    load_at(&path).get(claude_session_id).cloned()
}

/// Record the correlation (read-modify-write, atomic, 0o600).
///
/// # Errors
/// Path resolution or atomic-write failure.
pub fn record_session(claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()> {
    let path = state_path()?;
    record_at(&path, claude_session_id, icm_session_id)
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
