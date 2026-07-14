//! Best-effort reaping of session-log files whose mtime exceeds a configured
//! retention window. Run once at `SessionStart` when `transcript.retention_days`
//! is set.
//!
//! Only the current session-log file (`session-log.jsonl` in the state dir, or
//! whichever path the file sink is configured to use) is considered for expiry,
//! since the JSONL sink is a single append-only file. Empty parent directories
//! are removed after deletion.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Garbage-collect the session-log file at `log_path` when its mtime is older
/// than `retention_days`. Best-effort: failures are logged at `warn!` and
/// dropped — session logging never fails a launch.
///
/// The directory `log_path` lives in is cleaned up (removed if empty) after
/// deletion to prevent accumulation of empty state directories.
///
/// # Effects
/// Deletes the file on disk. No-op when the file does not exist, when its mtime
/// cannot be read, or when retention is zero (guarded by caller — the config
/// validator rejects `retention_days: 0`, but this function still handles it
/// defensively).
pub fn reap_session_log(log_path: &Path, retention_days: u64) {
    if retention_days == 0 {
        return;
    }
    if !log_path.try_exists().unwrap_or(false) {
        return;
    }
    let Ok(meta) = log_path.metadata() else {
        return;
    };
    let Ok(modified) = meta.modified() else {
        return;
    };
    let retention = Duration::from_secs(retention_days * 86_400);
    let Ok(now) = SystemTime::now().duration_since(modified) else {
        // File mtime is in the future — skip rather than delete.
        return;
    };
    if now < retention {
        return;
    }
    if let Err(e) = std::fs::remove_file(log_path) {
        tracing::warn!("session_log reaper: failed to remove {log_path:?}: {e}");
        return;
    }
    tracing::info!(
        path = %log_path.display(),
        retention_days,
        "session_log reaper: removed expired file"
    );
    // Best-effort cleanup of the parent directory if it became empty.
    if let Some(parent) = log_path.parent()
        && parent
            .read_dir()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(parent);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn does_not_delete_file_within_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        // File was just created, so mtime is now — retention of 1 day should
        // keep it.
        let mtime_before = path.metadata().unwrap().modified().unwrap();
        reap_session_log(&path, 1);
        assert!(path.exists(), "file within retention must not be deleted");
        let mtime_after = path.metadata().unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "mtime must not change");
    }

    #[test]
    fn deletes_file_exceeding_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();

        // Push the mtime back past retention.
        let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();

        reap_session_log(&path, 1);
        assert!(!path.exists(), "file past retention must be deleted");
    }

    #[test]
    fn noop_when_file_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.jsonl");
        reap_session_log(&path, 1);
        // No panic — the function should handle gracefully.
    }

    #[test]
    fn noop_when_retention_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        reap_session_log(&path, 0);
        assert!(path.exists(), "retention 0 must not delete");
    }

    #[test]
    fn cleans_up_empty_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        reap_session_log(&path, 1);
        assert!(!path.exists(), "file must be deleted");
        assert!(!dir.path().exists(), "empty parent dir must be removed");
    }

    #[test]
    fn noop_when_mtime_is_in_future() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let future = SystemTime::now() + Duration::from_secs(86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(future)).unwrap();
        reap_session_log(&path, 1);
        assert!(path.exists(), "file with future mtime must not be deleted");
    }
}
