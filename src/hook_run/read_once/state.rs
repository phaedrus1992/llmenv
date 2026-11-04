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

/// The cache key of `path`: its canonical path. A file that does not exist yet is keyed by its
/// canonical parent joined with its name, so every spelling of its folder gives one key. A path
/// that cannot be resolved even so is keyed by its raw form, and the agent is told (#2818).
pub(super) fn cache_key(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path)
        .or_else(|first| canonical_parent_join(path).map_err(|_| first))
        .inspect_err(|e| {
            // Hook stderr never reaches the agent, so the notice goes to the agent (#2818).
            crate::agent_notice::record(format!(
                "llmenv: read-once could not resolve {}: {e}. Repeat reads use the raw path as the \
                 key. Fix: check the file path and its parent directory.",
                path.display()
            ));
            tracing::debug!(
                error = %e,
                path = %path.display(),
                "cannot canonicalize read-once cache path, using raw path"
            )
        })
        .unwrap_or_else(|_| path.to_path_buf());
    resolved.to_string_lossy().to_string()
}

/// The canonical folder of `path`, joined with its file name. A missing file has no canonical
/// path of its own, so its folder carries the resolution. A dangling link is refused: its folder
/// does not name the target, and the target may appear later under a second key.
fn canonical_parent_join(path: &Path) -> std::io::Result<std::path::PathBuf> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    let name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok(std::fs::canonicalize(parent)?.join(name))
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
    fn the_cache_key_of_a_missing_file_in_an_existing_folder_is_the_canonical_parent_and_name() {
        let dir = tempfile::tempdir().unwrap();
        // The dotted spelling differs from its canonical form on every machine, so the test
        // cannot pass by accident when the raw path equals the canonical one.
        let dotted = dir.path().join(".").join("nope.txt");
        let expected = dir.path().canonicalize().unwrap().join("nope.txt");
        assert_eq!(cache_key(&dotted), expected.to_string_lossy());
    }

    #[test]
    fn two_spellings_of_a_missing_file_share_one_cache_key() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("nope.txt");
        let dotted = dir.path().join(".").join("nope.txt");
        assert_eq!(cache_key(&plain), cache_key(&dotted));
    }

    #[test]
    fn a_missing_file_in_a_missing_folder_is_noticed_and_keyed_by_the_raw_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-folder").join("nope.txt");
        assert_eq!(cache_key(&missing), missing.to_string_lossy());
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(text.contains(&missing.display().to_string()), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_keyed_by_its_raw_path_and_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(dir.path().join("target.txt"), &link).unwrap();

        assert_eq!(cache_key(&link), link.to_string_lossy());
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(text.contains(&link.display().to_string()), "{text}");
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
