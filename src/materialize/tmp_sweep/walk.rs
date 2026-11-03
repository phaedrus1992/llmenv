//! The walk of `state/tmp`: it sorts every entry into stale, kept, or failed, and changes
//! nothing. The sweep and the summary both build on it.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

use super::claude_root::{is_split_root, scan_split_root};
use crate::materialize::cache::{AgeSource, file_stamp, is_expired, newest_file_mtime};
use crate::materialize::expiry::AgeRule;

/// One entry that the scan found old enough to remove, and not in use.
pub(super) struct Stale {
    pub(super) path: PathBuf,
    pub(super) is_dir: bool,
}

/// What a scan of `state/tmp` found. Nothing is removed by a scan.
#[derive(Default)]
pub(super) struct Scan {
    pub(super) stale: Vec<Stale>,
    pub(super) kept: usize,
    pub(super) failed: Vec<PathBuf>,
}

/// Walk `tmp_root` and sort every entry into stale, kept, or failed. The walk changes nothing.
pub(super) fn scan(tmp_root: &Path, rule: AgeRule, in_use: &[PathBuf]) -> anyhow::Result<Scan> {
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
            scan_split_root(&path, rule, in_use, now, &mut scan);
            continue;
        }
        judge_entry(&path, is_dir, rule, now, &mut scan);
    }
    Ok(scan)
}

/// Whether `error` is a missing entry, anywhere in its chain. A `NotFound` is the entry being
/// removed while the walk runs. Other errors stay failures.
fn is_vanished(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Mark one entry stale when its newest file mtime is older than `older_than`. Keep it otherwise.
pub(super) fn judge_entry(
    path: &Path,
    is_dir: bool,
    rule: AgeRule,
    now: SystemTime,
    scan: &mut Scan,
) {
    let modified = match entry_mtime(path, is_dir, rule.source) {
        Ok(modified) => modified,
        // Another process removed the entry after the walk listed it. That is not a failure,
        // the same as a child that vanishes inside a folder walk (#2835, #3276).
        Err(e) if is_vanished(&e) => {
            tracing::debug!(path = %path.display(), "state/tmp entry removed during the walk");
            return;
        }
        Err(e) => {
            record_failure(path, &e, scan);
            return;
        }
    };
    // `is_expired` keeps a future mtime and logs the skew. A removal on a bad clock is not
    // reversible, so the entry must reach that check.
    if !is_expired(now, modified, rule.older_than, path) {
        scan.kept += 1;
    } else {
        scan.stale.push(Stale {
            path: path.to_path_buf(),
            is_dir,
        });
    }
}

/// The newest file mtime of an entry. A symlink is judged by its own mtime and never followed.
fn entry_mtime(path: &Path, is_dir: bool, source: AgeSource) -> anyhow::Result<SystemTime> {
    if is_dir {
        return newest_file_mtime(path, source);
    }
    std::fs::symlink_metadata(path)
        .and_then(|meta| file_stamp(&meta, source))
        .with_context(|| format!("stat {}", path.display()))
}

/// Record an entry the sweep could not read or judge. The entry stays, and the next run retries.
pub(super) fn record_failure(path: &Path, error: impl std::fmt::Display, scan: &mut Scan) {
    tracing::error!(
        path = %path.display(),
        error = %error,
        "state/tmp sweep: cannot read entry; skipping"
    );
    scan.failed.push(path.to_path_buf());
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: a failed setup step should abort the test"
)]
mod tests {
    use std::fs::File;

    use super::*;
    use crate::materialize::tmp_sweep::tests::DAY;
    use crate::test_log_capture::capture_logs;

    #[test]
    fn a_future_mtime_entry_is_kept_and_its_clock_skew_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("future.txt");
        std::fs::write(&file, b"f").unwrap();
        File::open(&file)
            .unwrap()
            .set_modified(SystemTime::now() + DAY)
            .unwrap();

        let mut scan = Scan::default();
        let logs = capture_logs(|| {
            judge_entry(
                &file,
                false,
                AgeRule {
                    older_than: DAY * 7,
                    source: AgeSource::MtimeOnly,
                },
                SystemTime::now(),
                &mut scan,
            );
        });

        assert!(scan.stale.is_empty(), "a future mtime must not be stale");
        assert_eq!(scan.kept, 1);
        assert!(logs.contains("clock skew"), "skew not logged: {logs}");
    }
}
