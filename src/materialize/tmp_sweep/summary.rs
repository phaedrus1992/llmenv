//! The count and size of the entries that the sweep would remove. Nothing is removed here.

use std::path::{Path, PathBuf};

use crate::materialize::dir_size::{DirSize, dir_size};
use crate::materialize::expiry::AgeRule;

use super::StaleSummary;
use super::walk::scan;

/// Count and size the entries that [`sweep`](super::sweep) would remove, without removing any.
/// `in_use` gives the paths that are in use, as
/// [`open_paths`](crate::materialize::open_files::open_paths) does.
///
/// The in-use list only removes entries from the stale count, so a scan with no in-use list
/// gives the upper bound. When that bound holds no stale and no unreadable entry, the answer
/// is final and `in_use` never runs. A `SessionStart` hook needs this: `lsof` lists every open
/// file on the host, and it can take seconds (#3031).
///
/// # Errors
/// Fails when the state folder cannot be read, or when `in_use` fails.
pub(crate) fn stale_summary_with(
    state_dir: &Path,
    rule: AgeRule,
    in_use: impl FnOnce() -> anyhow::Result<Vec<PathBuf>>,
) -> anyhow::Result<StaleSummary> {
    let tmp_root = state_dir.join("tmp");
    // One clock for both scans, so the bound and the answer judge the same ages.
    let now = std::time::SystemTime::now();
    let bound = stale_summary_in(&tmp_root, rule, &[], now)?;
    if bound.count == 0 && bound.unreadable == 0 {
        return Ok(bound);
    }
    stale_summary_in(&tmp_root, rule, &in_use()?, now)
}

/// The summary for `tmp_root`, with the in-use list given by the caller, judged against `now`.
///
/// # Errors
/// Fails when `tmp_root` cannot be read or resolved.
pub fn stale_summary_in(
    tmp_root: &Path,
    rule: AgeRule,
    in_use: &[PathBuf],
    now: std::time::SystemTime,
) -> anyhow::Result<StaleSummary> {
    let scan = scan(tmp_root, rule, in_use, now)?;
    // A stale entry whose size cannot be read is counted as unreadable, and the other entries
    // keep their sizes, so one bad folder does not hide the whole summary (#3277).
    let mut unsized_entries = 0usize;
    let total = scan
        .stale
        .iter()
        .fold(DirSize::default(), |total, s| match dir_size(&s.path) {
            Ok(size) => total.combine(size),
            Err(e) => {
                tracing::error!("cannot size the stale entry {}: {e:#}", s.path.display());
                unsized_entries += 1;
                total
            }
        });
    Ok(StaleSummary {
        count: scan.stale.len(),
        bytes: total.bytes,
        bytes_partial: total.skipped > 0 || unsized_entries > 0,
        unreadable: scan.failed.len() + unsized_entries,
    })
}
