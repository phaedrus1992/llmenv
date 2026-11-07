//! The per-entry steps of a cache walk: list one entry, and treat a vanished path as done.

use std::path::{Path, PathBuf};

/// The path and type of one listed entry, read without following a symlink. An entry that cannot
/// be read is logged and skipped, so the pass goes on. The failure goes to `fail` with the path
/// (`dir` when the entry has no path) and the cause, so a gc report can name the cause (#3441).
pub(super) fn entry_and_type(
    dir: &Path,
    entry: std::io::Result<std::fs::DirEntry>,
    mut fail: impl FnMut(PathBuf, String),
) -> Option<(PathBuf, std::fs::FileType)> {
    let entry = match entry {
        Ok(entry) => entry,
        Err(e) => {
            let cause = format!("cannot read an entry of {}: {e}", dir.display());
            tracing::error!("cache gc: {cause}");
            fail(dir.to_path_buf(), cause);
            return None;
        }
    };
    let p = entry.path();
    match entry.file_type() {
        Ok(ft) => Some((p, ft)),
        // Another process removed the entry after the listing (#3423).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %p.display(), "cache gc: entry removed during the walk");
            None
        }
        Err(e) => {
            let cause = format!(
                "cannot read the type of {}, so it is kept: {e}",
                p.display()
            );
            tracing::error!("cache gc: {cause}");
            fail(p, cause);
            None
        }
    }
}

/// Whether `error` is a missing entry, anywhere in its chain. A `NotFound` is the entry being
/// removed while a walk runs. Other errors stay failures (#2835, #3423).
pub(crate) fn is_vanished(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// `None` for a path that no longer exists. Any other error stays an error.
pub(super) fn skip_vanished<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `p` itself is gone from the disk. A NotFound from a child of a folder that still exists
/// is not a removal, because the folder is still there to be seen and counted (#3509).
pub(crate) fn is_gone(p: &Path) -> bool {
    matches!(std::fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_entry_reaches_the_callback_with_its_cause() {
        let dir = Path::new("/cache");
        let mut got = Vec::new();
        let entry = Err(std::io::Error::other("disk gone"));
        assert!(entry_and_type(dir, entry, |p, cause| got.push((p, cause))).is_none());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, dir);
        assert!(got[0].1.contains("disk gone"), "{got:?}");
        assert!(got[0].1.contains("/cache"), "{got:?}");
    }
}
