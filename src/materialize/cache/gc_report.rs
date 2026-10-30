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
    /// Entries gc tried to remove but couldn't. Same contract as
    /// [`super::PruneReport::failed`] — never counted as `removed` (#1372).
    pub(crate) failed: Vec<PathBuf>,
    /// The reason for each entry in `failed`, keyed by path, so the summary can name it (#3077).
    pub(crate) causes: BTreeMap<PathBuf, String>,
    /// When set, the sweep records each removal in `removed` and deletes nothing (#2917).
    pub(crate) dry_run: bool,
}

impl GcReport {
    /// An empty report for a sweep that deletes, or only previews when `dry_run` is set.
    pub(crate) fn new(dry_run: bool) -> Self {
        Self {
            removed: Vec::new(),
            kept: 0,
            failed: Vec::new(),
            causes: BTreeMap::new(),
            dry_run,
        }
    }

    /// Record `path` as failed, with the reason the sweep gave (#3077).
    pub(crate) fn record_failure(&mut self, path: PathBuf, cause: String) {
        self.causes.insert(path.clone(), cause);
        self.failed.push(path);
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
    match remove(p) {
        Ok(()) => report.removed.push(p.to_path_buf()),
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
        let missing = dir.path().join("absent");
        let mut report = GcReport::new(false);

        gc_remove_link(&missing, &mut report);

        assert_eq!(report.failed, vec![missing.clone()]);
        assert!(report.causes.get(&missing).is_some_and(|c| !c.is_empty()));
    }
}
