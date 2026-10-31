//! Age sweep of the state temp folder, `state/tmp` (#2814).
//!
//! `TMPDIR` points at this folder, so every tool that honors it writes here. Nothing else
//! removes what those tools leave behind. An entry is removed when its newest mtime is older
//! than the retention, and no process holds a path inside it open.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::materialize::cache::{GcReport, gc_remove_dir, gc_remove_link};
use crate::materialize::dir_size::{DirSize, dir_size};
use crate::materialize::open_files::open_paths;

mod claude_root;

mod walk;
use walk::scan;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs::{self, File};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::{StaleSummary, stale_summary_in, stale_summary_with, sweep};

    pub(super) const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn summary_never_asks_lsof_when_no_entry_is_a_candidate() {
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(state.path().join("tmp").join("fresh")).unwrap();
        let summary = stale_summary_with(state.path(), DAY, || {
            anyhow::bail!("lsof must not run when nothing is stale")
        })
        .unwrap();
        assert_eq!(summary.count, 0);
    }

    #[test]
    fn summary_asks_lsof_when_an_entry_is_stale() {
        let state = tempfile::tempdir().unwrap();
        let entry = state.path().join("tmp").join("old");
        fs::create_dir_all(&entry).unwrap();
        age_tree(&entry, SystemTime::now() - 2 * DAY);
        let summary = stale_summary_with(state.path(), DAY, || Ok(Vec::new())).unwrap();
        assert_eq!(summary.count, 1);
    }

    #[test]
    fn summary_keeps_a_stale_entry_that_lsof_reports_in_use() {
        let state = tempfile::tempdir().unwrap();
        let entry = state.path().join("tmp").join("busy");
        fs::create_dir_all(&entry).unwrap();
        age_tree(&entry, SystemTime::now() - 2 * DAY);
        let in_use = vec![fs::canonicalize(&entry).unwrap()];
        let summary = stale_summary_with(state.path(), DAY, || Ok(in_use)).unwrap();
        assert_eq!(summary.count, 0);
    }

    /// Set the mtime of `path` and of everything under it, so the newest mtime is `when`.
    pub(super) fn age_tree(path: &Path, when: SystemTime) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                age_tree(&entry.unwrap().path(), when);
            }
        }
        File::open(path).unwrap().set_modified(when).unwrap();
    }

    pub(super) fn old() -> SystemTime {
        SystemTime::now() - 30 * DAY
    }

    /// Every path under `root`, so a test can tell that a run changed nothing.
    fn listing(root: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
        let mut out = std::collections::BTreeSet::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(listing(&path));
            }
            out.insert(path);
        }
        out
    }

    #[test]
    fn a_dry_run_lists_exactly_what_a_real_run_removes() {
        use proptest::prelude::*;
        let entry = (any::<bool>(), any::<bool>(), any::<bool>());
        proptest!(ProptestConfig::with_cases(24), |(entries in proptest::collection::vec(entry, 0..8))| {
            let root = tempfile::tempdir().unwrap();
            let resolved = fs::canonicalize(root.path()).unwrap();
            let mut in_use = Vec::new();
            for (i, (is_dir, is_old, held)) in entries.iter().enumerate() {
                let path = root.path().join(format!("e{i}"));
                if *is_dir {
                    fs::create_dir(&path).unwrap();
                    fs::write(path.join("f"), b"x").unwrap();
                } else {
                    fs::write(&path, b"x").unwrap();
                }
                if *is_old {
                    age_tree(&path, old());
                }
                if *held {
                    in_use.push(resolved.join(format!("e{i}")));
                }
            }
            let before = listing(root.path());

            let preview = sweep(root.path(), DAY * 7, &in_use, true).unwrap();
            prop_assert_eq!(&listing(root.path()), &before);
            let real = sweep(root.path(), DAY * 7, &in_use, false).unwrap();

            let mut listed = preview.removed.clone();
            listed.sort();
            let mut removed = real.removed.clone();
            removed.sort();
            prop_assert_eq!(listed, removed);
            prop_assert!(real.failed.is_empty());
        });
    }

    #[test]
    fn a_dry_run_lists_the_old_entries_and_removes_none() {
        let root = tempfile::tempdir().unwrap();
        let old_dir = root.path().join("cargo-old");
        fs::create_dir(&old_dir).unwrap();
        fs::write(old_dir.join("artifact"), b"x").unwrap();
        age_tree(&old_dir, old());
        let old_file = root.path().join("scratch.txt");
        fs::write(&old_file, b"y").unwrap();
        age_tree(&old_file, old());
        fs::create_dir(root.path().join("fresh")).unwrap();

        let report = sweep(root.path(), DAY * 7, &[], true).unwrap();

        let mut listed = report.removed.clone();
        listed.sort();
        let mut expected = vec![old_dir.clone(), old_file.clone()];
        expected.sort();
        assert_eq!(listed, expected);
        assert_eq!(report.kept, 1);
        assert!(old_dir.exists() && old_file.exists());
    }

    #[test]
    fn an_old_directory_and_an_old_file_are_removed_and_a_fresh_one_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let old_dir = root.path().join("cargo-old");
        fs::create_dir(&old_dir).unwrap();
        fs::write(old_dir.join("artifact"), b"x").unwrap();
        age_tree(&old_dir, old());
        let old_file = root.path().join("scratch.txt");
        fs::write(&old_file, b"y").unwrap();
        age_tree(&old_file, old());
        let fresh = root.path().join("fresh");
        fs::create_dir(&fresh).unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert_eq!(report.removed.len(), 2, "removed: {:?}", report.removed);
        assert_eq!(report.kept, 1);
        assert!(!old_dir.exists());
        assert!(!old_file.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn an_old_entry_with_a_path_in_use_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let busy = root.path().join("cargo-busy");
        fs::create_dir(&busy).unwrap();
        let held = busy.join("open.log");
        fs::write(&held, b"z").unwrap();
        age_tree(&busy, old());

        let report = sweep(root.path(), DAY * 7, std::slice::from_ref(&held), false).unwrap();

        assert!(
            report.removed.is_empty(),
            "removed a busy entry: {:?}",
            report.removed
        );
        assert_eq!(report.kept, 1);
        assert!(held.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_in_use_path_matches_when_the_root_is_reached_through_a_symlink() {
        let real = tempfile::tempdir().unwrap();
        let busy = real.path().join("cargo-busy");
        fs::create_dir(&busy).unwrap();
        let held = busy.join("open.log");
        fs::write(&held, b"z").unwrap();
        age_tree(&busy, old());
        let linked_dir = tempfile::tempdir().unwrap();
        let link = linked_dir.path().join("tmp-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        // lsof reports the resolved path, so the in-use list holds it, not the link path.
        let report = sweep(&link, DAY * 7, std::slice::from_ref(&held), false).unwrap();

        assert!(
            report.removed.is_empty(),
            "removed a busy entry: {:?}",
            report.removed
        );
        assert!(held.exists());
    }

    #[test]
    fn an_entry_with_a_future_mtime_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let future = root.path().join("future.txt");
        fs::write(&future, b"f").unwrap();
        File::open(&future)
            .unwrap()
            .set_modified(SystemTime::now() + DAY)
            .unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert!(report.removed.is_empty());
        assert!(future.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_entry_is_recorded_and_the_rest_are_still_swept() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("inner"), b"i").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let old_file = root.path().join("old.txt");
        fs::write(&old_file, b"o").unwrap();
        age_tree(&old_file, old());

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        // Restore the mode before any assertion, so the tempdir can be removed.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            report.failed,
            vec![root.path().canonicalize().unwrap().join("locked")]
        );
        assert_eq!(report.removed.len(), 1, "removed: {:?}", report.removed);
        assert!(!old_file.exists());
    }

    /// The link is new, so only a zero retention makes it expired. The project's other GC
    /// tests use the same zero retention.
    #[cfg(unix)]
    #[test]
    fn an_expired_symlink_is_removed_and_its_target_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("keep-me");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("data"), b"d").unwrap();
        let tmp = root.path().join("tmp");
        fs::create_dir(&tmp).unwrap();
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let report = sweep(&tmp, Duration::ZERO, &[], false).unwrap();

        assert_eq!(
            report.removed,
            vec![tmp.canonicalize().unwrap().join("link")]
        );
        assert!(link.symlink_metadata().is_err(), "the link survived");
        assert!(target.join("data").exists(), "the link target was followed");
    }

    #[test]
    fn the_summary_counts_and_sizes_the_stale_entries_and_removes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let old_dir = root.path().join("cargo-old");
        fs::create_dir(&old_dir).unwrap();
        fs::write(old_dir.join("a"), [0_u8; 300]).unwrap();
        fs::write(old_dir.join("b"), [0_u8; 200]).unwrap();
        age_tree(&old_dir, old());
        let fresh = root.path().join("fresh");
        fs::create_dir(&fresh).unwrap();
        fs::write(fresh.join("x"), [0_u8; 999]).unwrap();

        let summary = stale_summary_in(root.path(), DAY * 7, &[]).unwrap();

        assert_eq!(
            summary,
            StaleSummary {
                count: 1,
                bytes: 500,
                bytes_partial: false,
                unreadable: 0
            }
        );
        assert!(old_dir.join("a").exists(), "the summary removed an entry");
    }

    #[test]
    fn the_summary_skips_a_claude_code_root_and_an_entry_in_use() {
        let root = tempfile::tempdir().unwrap();
        let claude = root.path().join("claude-501");
        fs::create_dir(&claude).unwrap();
        age_tree(&claude, old());
        let busy = root.path().join("cargo-busy");
        fs::create_dir(&busy).unwrap();
        age_tree(&busy, old());

        let summary = stale_summary_in(root.path(), DAY * 7, std::slice::from_ref(&busy)).unwrap();

        assert_eq!(
            summary,
            StaleSummary {
                count: 0,
                bytes: 0,
                bytes_partial: false,
                unreadable: 0
            }
        );
    }

    #[test]
    fn the_summary_of_a_missing_tmp_root_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("no-such-tmp");

        let summary = stale_summary_in(&missing, DAY * 7, &[]).unwrap();

        assert_eq!(
            summary,
            StaleSummary {
                count: 0,
                bytes: 0,
                bytes_partial: false,
                unreadable: 0
            }
        );
    }

    /// Set the mtime of the folders under `path`, and of `path`, to now. Files keep theirs.
    fn touch_dirs(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let child = entry.unwrap().path();
            if child.is_dir() {
                touch_dirs(&child);
            }
        }
        File::open(path)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
    }

    // #3184: a partial delete or a scan changes folder mtimes, so they do not show use.
    #[test]
    fn an_entry_with_old_files_and_fresh_folders_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let entry = root.path().join("build");
        fs::create_dir_all(entry.join("deep").join("deeper")).unwrap();
        fs::write(entry.join("deep").join("f"), b"x").unwrap();
        age_tree(&entry, old());
        touch_dirs(&entry);

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert_eq!(
            report.removed,
            vec![fs::canonicalize(root.path()).unwrap().join("build")]
        );
    }

    #[test]
    fn an_entry_with_a_fresh_file_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let entry = root.path().join("build");
        fs::create_dir_all(entry.join("deep")).unwrap();
        fs::write(entry.join("deep").join("old"), b"x").unwrap();
        age_tree(&entry, old());
        fs::write(entry.join("deep").join("new"), b"x").unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert!(report.removed.is_empty(), "{:?}", report.removed);
    }

    #[test]
    fn a_fresh_folder_tree_with_no_file_is_kept() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("empty").join("sub")).unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert!(report.removed.is_empty(), "{:?}", report.removed);
    }

    #[test]
    fn an_old_folder_tree_with_no_file_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let entry = root.path().join("empty");
        fs::create_dir_all(entry.join("sub")).unwrap();
        age_tree(&entry, old());

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert_eq!(report.removed.len(), 1);
    }

    // #3184: one fresh workspace build must not keep the old builds beside it.
    #[test]
    fn each_build_in_the_hawk_target_root_is_judged_alone() {
        let root = tempfile::tempdir().unwrap();
        let hawk = root.path().join("cargo-hawk-target");
        for name in ["old-build", "fresh-build"] {
            fs::create_dir_all(hawk.join(name)).unwrap();
            fs::write(hawk.join(name).join("f"), b"x").unwrap();
        }
        age_tree(&hawk, old());
        fs::write(hawk.join("fresh-build").join("g"), b"x").unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        let resolved = fs::canonicalize(&hawk).unwrap();
        assert_eq!(report.removed, vec![resolved.join("old-build")]);
        assert!(hawk.join("fresh-build").exists());
    }

    #[test]
    fn an_old_hawk_target_root_is_kept_when_empty() {
        let root = tempfile::tempdir().unwrap();
        let hawk = root.path().join("cargo-hawk-target");
        fs::create_dir(&hawk).unwrap();
        age_tree(&hawk, old());

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert!(report.removed.is_empty());
        assert!(hawk.exists());
    }
}
