//! Where the read-once cache lives, and how it is cleared.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Build the state subdirectory path for read-once cache files.
pub fn read_once_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("read_once")
}

/// Build the full path to a session's cache file.
pub(in crate::hook_run) fn session_cache_path(state_dir: &Path, session_id: &str) -> PathBuf {
    read_once_state_dir(state_dir).join(format!("{session_id}.json"))
}

/// Clear the read-once cache under `state_dir` by removing its folder, and say on stdout what
/// happened.
pub fn clear_cache(state_dir: &Path) -> anyhow::Result<()> {
    let ro_dir = read_once_state_dir(state_dir);
    // Judge the entry itself. `exists()` follows a link, so a dangling link reported "no cache"
    // and stayed in place (#3116).
    match std::fs::symlink_metadata(&ro_dir) {
        Ok(meta) => {
            if meta.is_dir() {
                std::fs::remove_dir_all(&ro_dir)?;
            } else {
                std::fs::remove_file(&ro_dir)?;
            }
            writeln!(std::io::stdout(), "Read-once cache cleared")?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            writeln!(std::io::stdout(), "No read-once cache to clear")?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// The cache key of `path`: its canonical path. A path that cannot be resolved is keyed by its
/// raw form, and the agent is told (#2818).
pub(super) fn cache_key(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path)
        .inspect_err(|e| {
            // Hook stderr never reaches the agent, so the notice goes to the agent (#2818).
            crate::agent_notice::record(format!(
                "llmenv: read-once could not resolve {}: {e}. Repeat reads use the raw path as the \
             key. Fix: check the file path and its parent directory.",
                path.display()
            ));
            // A path that does not exist yet is valid input, and the notice above already tells
            // the agent, so the log is debug only.
            tracing::debug!(
                error = %e,
                path = %path.display(),
                "cannot canonicalize read-once cache path, using raw path"
            )
        })
        .unwrap_or_else(|_| path.to_path_buf());
    canonical.to_string_lossy().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{cache_key, clear_cache, read_once_state_dir};

    #[test]
    fn clear_cache_removes_the_read_once_folder_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let ro = read_once_state_dir(dir.path());
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("s.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"k").unwrap();

        clear_cache(dir.path()).unwrap();

        assert!(!ro.exists());
        assert!(dir.path().join("keep.txt").exists());
    }

    #[test]
    fn clear_cache_with_no_folder_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        clear_cache(dir.path()).unwrap();
    }

    // Only "not found" means no cache. Any other lookup failure is an error. Here the cache
    // folder's parent is a regular file, so the lookup fails with ENOTDIR.
    #[cfg(unix)]
    #[test]
    fn clear_cache_reports_a_failed_lookup_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("read_once");
        std::fs::write(&state, "not a folder").unwrap();

        assert!(clear_cache(&state).is_err());
    }

    // #3116: `exists()` follows the link, so a dangling link at the folder reported no cache.
    #[cfg(unix)]
    #[test]
    fn clear_cache_removes_a_dangling_link_at_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ro = read_once_state_dir(dir.path());
        std::os::unix::fs::symlink(dir.path().join("gone"), &ro).unwrap();

        clear_cache(dir.path()).unwrap();

        assert!(
            std::fs::symlink_metadata(&ro).is_err(),
            "the dangling link must be removed"
        );
    }

    #[test]
    fn the_cache_key_of_an_existing_file_is_its_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"a").unwrap();
        assert_eq!(
            cache_key(&file),
            file.canonicalize().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn the_cache_key_of_a_missing_path_is_the_raw_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");
        assert_eq!(cache_key(&missing), missing.to_string_lossy());
    }

    #[test]
    fn a_missing_path_writes_no_warning_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.jsonl");
        let missing = dir.path().join("nope.txt");
        // A path that does not exist yet is valid input, so only the agent notice is raised.
        let _ = crate::session_log::tracing_layer::capture_file_logs_at(
            &log,
            tracing_subscriber::filter::LevelFilter::WARN,
            || cache_key(&missing),
        );
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!body.contains("cannot canonicalize"), "{body}");
    }
}
