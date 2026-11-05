//! The report of an age-based sweep, and the two removal helpers that fill it (#1372, #2917).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::remove_dir_all_forcing_writable;

/// No `Default`: each sweep must choose `dry_run` through [`GcReport::new`], so a new sweep cannot
/// delete by accident when its caller asked for a preview.
#[derive(Debug)]
pub struct GcReport {
    pub removed: Vec<PathBuf>,
    pub kept: usize,
    /// Entries gc tried to remove but couldn't, each with the reason the summary names (#3077).
    /// Same contract as [`super::PruneReport::failed`]: never counted as `removed` (#1372). A map,
    /// so a path is listed once and always has a cause (#3441).
    pub(crate) failed: BTreeMap<PathBuf, String>,
    /// When set, the sweep records each removal in `removed` and deletes nothing (#2917).
    pub(crate) dry_run: bool,
}

impl GcReport {
    /// An empty report for a sweep that deletes, or only previews when `dry_run` is set.
    pub(crate) fn new(dry_run: bool) -> Self {
        Self {
            removed: Vec::new(),
            kept: 0,
            failed: BTreeMap::new(),
            dry_run,
        }
    }

    /// Record `path` as failed, with the reason the sweep gave (#3077). A second failure of the
    /// same path keeps the first cause.
    pub(crate) fn record_failure(&mut self, path: PathBuf, cause: String) {
        self.failed.entry(path).or_insert(cause);
    }
}

/// Remove a directory for [`gc`](super::gc), recording rather than propagating a failure.
///
/// Same reasoning as [`remove_dir`](super::remove_dir) (#1372): one undeletable entry must not
/// abort collection of everything after it, since `doctor --gc` reported only
/// the bare errno and left the rest of the cache untouched.
///
/// A removal that stops part-way names a path in the tree that another user owns, with the fix
/// (#3184).
pub(crate) fn gc_remove_dir(p: &Path, report: &mut GcReport) {
    gc_remove(
        p,
        report,
        "remove cache directory",
        remove_dir_all_forcing_writable,
    );
}

/// Unlink a symlink or a file for [`gc`](super::gc), recording rather than propagating a failure.
pub(crate) fn gc_remove_link(p: &Path, report: &mut GcReport) {
    gc_remove(p, report, "unlink cache entry", |p| std::fs::remove_file(p));
}

/// Record `p` as removed. A dry run stops there; a real run calls `remove` first, and records a
/// failure instead when it fails. The cause names a path that another user owns (#3184).
fn gc_remove(
    p: &Path,
    report: &mut GcReport,
    action: &str,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) {
    if report.dry_run {
        report.removed.push(p.to_path_buf());
        return;
    }
    // An entry that another process removed first is gone, which is the goal (#3423).
    match super::skip_vanished(remove(p)) {
        Ok(_) => report.removed.push(p.to_path_buf()),
        Err(e) => {
            let cause = super::owner::removal_cause(p, &e);
            tracing::error!(
                path = %p.display(),
                error = %cause,
                "gc: failed to {action}; skipping"
            );
            report.record_failure(p.to_path_buf(), cause);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_removal_keeps_its_cause_with_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A path under a regular file fails with "not a directory", not "not found".
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").expect("write fixture");
        let blocked = file.join("child");
        let mut report = GcReport::new(false);

        gc_remove_link(&blocked, &mut report);

        assert_eq!(report.failed.keys().collect::<Vec<_>>(), vec![&blocked]);
        assert!(report.failed.get(&blocked).is_some_and(|c| !c.is_empty()));
    }

    // An entry that another process removed first is gone, not a failure (#3423).
    #[test]
    fn an_entry_removed_by_another_process_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("absent");
        let mut report = GcReport::new(false);

        gc_remove_link(&gone, &mut report);
        gc_remove_dir(&gone, &mut report);

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.removed, vec![gone.clone(), gone]);
    }

    /// A path that fails twice is listed once, with its first cause (#3441).
    #[test]
    fn a_path_that_fails_twice_keeps_its_first_cause() {
        let mut report = GcReport::new(false);
        report.record_failure(PathBuf::from("a"), "first".to_string());
        report.record_failure(PathBuf::from("b"), "other".to_string());
        report.record_failure(PathBuf::from("a"), "second".to_string());
        assert_eq!(report.failed.len(), 2);
        assert_eq!(report.failed[Path::new("a")], "first");
    }
}
