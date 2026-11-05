//! Removal of the entries that the walk marked stale. The walk itself changes nothing.

use std::path::{Path, PathBuf};

use crate::materialize::cache::{GcReport, gc_remove_dir, gc_remove_link};
use crate::materialize::expiry::AgeRule;

use super::walk::scan;

/// Remove the entries of `tmp_root` that the rule expires and that are not in use.
///
/// `in_use` lists paths that a process holds open. An entry that contains one of them is kept,
/// because removing it would break a live process. lsof reports resolved paths, so the root is
/// resolved too. Otherwise a symlink on the way to the root hides every match.
///
/// A failure on one entry is recorded and the sweep goes on, so one unreadable entry does not
/// block the others (#1372).
///
/// With `dry_run`, the report lists the same entries and nothing is removed (#2917). Each entry is
/// judged against `now`, so a caller can judge several sweeps by one clock (#3374).
pub fn sweep(
    tmp_root: &Path,
    rule: AgeRule,
    in_use: &[PathBuf],
    dry_run: bool,
    now: std::time::SystemTime,
) -> anyhow::Result<GcReport> {
    let scan = scan(tmp_root, rule, in_use, now)?;
    let mut report = GcReport {
        kept: scan.kept,
        failed: scan.failed,
        ..GcReport::new(dry_run)
    };
    for stale in &scan.stale {
        if stale.is_dir {
            gc_remove_dir(&stale.path, &mut report);
        } else {
            gc_remove_link(&stale.path, &mut report);
        }
    }
    Ok(report)
}
