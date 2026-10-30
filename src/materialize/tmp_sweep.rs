//! Age sweep of the state temp folder, `state/tmp` (#2814).
//!
//! `TMPDIR` points at this folder, so every tool that honors it writes here. Nothing else
//! removes what those tools leave behind. An entry is removed when its newest mtime is older
//! than the retention, and no process holds a path inside it open.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;

use crate::materialize::cache::{
    GcReport, gc_remove_dir, gc_remove_link, is_expired, newest_file_mtime,
};
use crate::materialize::dir_size::{DirSize, dir_size};
use crate::materialize::open_files::open_paths;

mod claude_root;

use claude_root::{is_split_root, scan_split_root};

/// One entry that the scan found old enough to remove, and not in use.
struct Stale {
    path: PathBuf,
    is_dir: bool,
}

/// What a scan of `state/tmp` found. Nothing is removed by a scan.
#[derive(Default)]
struct Scan {
    stale: Vec<Stale>,
    kept: usize,
    failed: Vec<PathBuf>,
}

/// The stale entries of `state/tmp`: how many, and how many bytes they hold.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StaleSummary {
    pub count: usize,
    pub bytes: u64,
    /// True when `bytes` leaves out an entry that could not be measured, so it is a floor (#3069).
    pub bytes_partial: bool,
    /// Entries the scan could not read or judge. They are not in `count`.
    pub unreadable: usize,
}

/// Remove the entries of `tmp_root` that are older than `older_than` and not in use.
///
/// `in_use` lists paths that a process holds open. An entry that contains one of them is kept,
/// because removing it would break a live process. lsof reports resolved paths, so the root is
/// resolved too. Otherwise a symlink on the way to the root hides every match.
///
/// A failure on one entry is recorded and the sweep goes on, so one unreadable entry does not
/// block the others (#1372).
///
/// With `dry_run`, the report lists the same entries and nothing is removed (#2917).
pub fn sweep(
    tmp_root: &Path,
    older_than: Duration,
    in_use: &[PathBuf],
    dry_run: bool,
) -> anyhow::Result<GcReport> {
    let scan = scan(tmp_root, older_than, in_use)?;
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

/// Count and size the entries that [`sweep`] would remove, without removing any.
///
/// # Errors
/// Fails when the in-use list cannot be read. See [`open_paths`].
pub fn stale_summary(state_dir: &Path, older_than: Duration) -> anyhow::Result<StaleSummary> {
    stale_summary_with(state_dir, older_than, open_paths)
}

/// [`stale_summary`] with the in-use list supplied on demand.
///
/// The in-use list only removes entries from the stale count, so a scan with no in-use list
/// gives the upper bound. When that bound holds no stale and no unreadable entry, the answer
/// is final and `lsof` never runs. A `SessionStart` hook needs this: `lsof` lists every open
/// file on the host, and it can take seconds (#3031).
fn stale_summary_with(
    state_dir: &Path,
    older_than: Duration,
    in_use: impl FnOnce() -> anyhow::Result<Vec<PathBuf>>,
) -> anyhow::Result<StaleSummary> {
    let tmp_root = state_dir.join("tmp");
    let bound = stale_summary_in(&tmp_root, older_than, &[])?;
    if bound.count == 0 && bound.unreadable == 0 {
        return Ok(bound);
    }
    stale_summary_in(&tmp_root, older_than, &in_use()?)
}

/// The summary for `tmp_root`, with the in-use list given by the caller.
///
/// # Errors
/// Fails when `tmp_root` cannot be read or resolved.
pub fn stale_summary_in(
    tmp_root: &Path,
    older_than: Duration,
    in_use: &[PathBuf],
) -> anyhow::Result<StaleSummary> {
    let scan = scan(tmp_root, older_than, in_use)?;
    let sizes = scan
        .stale
        .iter()
        .map(|s| dir_size(&s.path))
        .collect::<anyhow::Result<Vec<DirSize>>>()?;
    let total = sizes.into_iter().fold(DirSize::default(), DirSize::combine);
    Ok(StaleSummary {
        count: scan.stale.len(),
        bytes: total.bytes,
        bytes_partial: total.skipped > 0,
        unreadable: scan.failed.len(),
    })
}

/// Walk `tmp_root` and sort every entry into stale, kept, or failed. The walk changes nothing.
fn scan(tmp_root: &Path, older_than: Duration, in_use: &[PathBuf]) -> anyhow::Result<Scan> {
    let mut scan = Scan::default();
    let Some(entries) = crate::paths::read_dir_optional(tmp_root)? else {
        return Ok(scan);
    };
    let root = std::fs::canonicalize(tmp_root)
        .with_context(|| format!("resolving {}", tmp_root.display()))?;
    let now = SystemTime::now();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                record_failure(&root, &e, &mut scan);
                continue;
            }
        };
        let name = entry.file_name();
        let path = root.join(&name);
        if in_use.iter().any(|used| used.starts_with(&path)) {
            scan.kept += 1;
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(e) => {
                record_failure(&path, &e, &mut scan);
                continue;
            }
        };
        // A root that holds unrelated entries is not judged as one entry, because one fresh entry
        // would keep all the others (#1379, #3184). Its entries are judged one by one instead.
        if is_dir && is_split_root(&name) {
            scan_split_root(&path, older_than, in_use, now, &mut scan);
            continue;
        }
        judge_entry(&path, is_dir, older_than, now, &mut scan);
    }
    Ok(scan)
}

/// Mark one entry stale when its newest file mtime is older than `older_than`. Keep it otherwise.
fn judge_entry(path: &Path, is_dir: bool, older_than: Duration, now: SystemTime, scan: &mut Scan) {
    let modified = match entry_mtime(path, is_dir) {
        Ok(modified) => modified,
        Err(e) => {
            record_failure(path, &e, scan);
            return;
        }
    };
    // `is_expired` keeps a future mtime and logs the skew. A removal on a bad clock is not
    // reversible, so the entry must reach that check.
    if !is_expired(now, modified, older_than, path) {
        scan.kept += 1;
    } else {
        scan.stale.push(Stale {
            path: path.to_path_buf(),
            is_dir,
        });
    }
}

/// The newest file mtime of an entry. A symlink is judged by its own mtime and never followed.
fn entry_mtime(path: &Path, is_dir: bool) -> anyhow::Result<SystemTime> {
    if is_dir {
        return newest_file_mtime(path);
    }
    std::fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .with_context(|| format!("stat {}", path.display()))
}

/// Record an entry the sweep could not read or judge. The entry stays, and the next run retries.
fn record_failure(path: &Path, error: impl std::fmt::Display, scan: &mut Scan) {
    tracing::error!(
        path = %path.display(),
        error = %error,
        "state/tmp sweep: cannot read entry; skipping"
    );
    scan.failed.push(path.to_path_buf());
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
