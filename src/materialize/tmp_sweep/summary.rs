//! The count and size of the entries that the sweep would remove. Nothing is removed here.

use std::path::{Path, PathBuf};

use crate::materialize::dir_size::{DirSize, dir_size};
use crate::materialize::expiry::AgeRule;
use crate::materialize::open_files::open_paths;

use super::StaleSummary;
use super::walk::scan;

/// Count and size the entries that [`sweep`](super::sweep) would remove, without removing any.
///
/// # Errors
/// Fails when the in-use list cannot be read. See [`open_paths`].
pub fn stale_summary(state_dir: &Path, rule: AgeRule) -> anyhow::Result<StaleSummary> {
    stale_summary_with(state_dir, rule, open_paths)
}

/// [`stale_summary`] with the in-use list supplied on demand.
///
/// The in-use list only removes entries from the stale count, so a scan with no in-use list
/// gives the upper bound. When that bound holds no stale and no unreadable entry, the answer
/// is final and `lsof` never runs. A `SessionStart` hook needs this: `lsof` lists every open
/// file on the host, and it can take seconds (#3031).
pub(super) fn stale_summary_with(
    state_dir: &Path,
    rule: AgeRule,
    in_use: impl FnOnce() -> anyhow::Result<Vec<PathBuf>>,
) -> anyhow::Result<StaleSummary> {
    let tmp_root = state_dir.join("tmp");
    let bound = stale_summary_in(&tmp_root, rule, &[])?;
    if bound.count == 0 && bound.unreadable == 0 {
        return Ok(bound);
    }
    stale_summary_in(&tmp_root, rule, &in_use()?)
}

/// The summary for `tmp_root`, with the in-use list given by the caller.
///
/// # Errors
/// Fails when `tmp_root` cannot be read or resolved.
pub fn stale_summary_in(
    tmp_root: &Path,
    rule: AgeRule,
    in_use: &[PathBuf],
) -> anyhow::Result<StaleSummary> {
    let scan = scan(tmp_root, rule, in_use)?;
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
