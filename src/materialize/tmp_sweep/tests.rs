use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::summary::{stale_summary_in, stale_summary_with};
use super::{StaleSummary, sweep};
use crate::materialize::cache::AgeSource;
use crate::materialize::expiry::AgeRule;

pub(super) const DAY: Duration = Duration::from_secs(86_400);

/// A real or dry sweep of `root` with a one-week mtime rule, judged by the clock.
fn sweep_week(root: &Path, dry_run: bool) -> anyhow::Result<crate::materialize::cache::GcReport> {
    sweep(root, mtime_rule(DAY * 7), &[], dry_run, SystemTime::now())
}

// Tests age a tree with `set_modified`, which cannot move the change time back. The mtime-only
// rule is the one that sees that age, so these tests use it (#3195).
pub(super) fn mtime_rule(older_than: Duration) -> AgeRule {
    AgeRule {
        older_than,
        source: AgeSource::MtimeOnly,
    }
}

#[test]
fn summary_never_asks_lsof_when_no_entry_is_a_candidate() {
    let state = tempfile::tempdir().unwrap();
    fs::create_dir_all(state.path().join("tmp").join("fresh")).unwrap();
    let summary = stale_summary_with(state.path(), mtime_rule(DAY), || {
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
    let summary = stale_summary_with(state.path(), mtime_rule(DAY), || Ok(Vec::new())).unwrap();
    assert_eq!(summary.count, 1);
}

#[test]
fn summary_keeps_a_stale_entry_that_lsof_reports_in_use() {
    let state = tempfile::tempdir().unwrap();
    let entry = state.path().join("tmp").join("busy");
    fs::create_dir_all(&entry).unwrap();
    age_tree(&entry, SystemTime::now() - 2 * DAY);
    let in_use = vec![fs::canonicalize(&entry).unwrap()];
    let summary = stale_summary_with(state.path(), mtime_rule(DAY), || Ok(in_use)).unwrap();
    assert_eq!(summary.count, 0);
}

// `set_modified` also sets the change time to now, the same as an unpack that keeps old
// mtimes. The change-time rule therefore keeps the tree, and the mtime-only rule removes it.
#[test]
fn an_unpacked_tree_with_old_mtimes_is_kept_by_the_change_time_rule() {
    let root = tempfile::tempdir().unwrap();
    let unpacked = root.path().join("unpacked");
    fs::create_dir(&unpacked).unwrap();
    fs::write(unpacked.join("artifact"), b"x").unwrap();
    age_tree(&unpacked, old());
    let rule = AgeRule {
        older_than: DAY * 7,
        source: AgeSource::MtimeOrCtime,
    };
    let report = sweep(root.path(), rule, &[], false, SystemTime::now()).unwrap();
    assert!(report.removed.is_empty(), "removed {:?}", report.removed);
    assert!(unpacked.exists());
}

#[test]
fn the_mtime_only_rule_removes_an_unpacked_tree_with_old_mtimes() {
    let root = tempfile::tempdir().unwrap();
    let unpacked = root.path().join("unpacked");
    fs::create_dir(&unpacked).unwrap();
    fs::write(unpacked.join("artifact"), b"x").unwrap();
    age_tree(&unpacked, old());
    let report = sweep_week(root.path(), false).unwrap();
    assert_eq!(report.removed, vec![unpacked.clone()]);
    assert!(!unpacked.exists());
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

        let preview = sweep(root.path(), mtime_rule(DAY * 7), &in_use, true, SystemTime::now()).unwrap();
        prop_assert_eq!(&listing(root.path()), &before);
        let real = sweep(root.path(), mtime_rule(DAY * 7), &in_use, false, SystemTime::now()).unwrap();

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

    let report = sweep_week(root.path(), true).unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

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

    let report = sweep(
        root.path(),
        mtime_rule(DAY * 7),
        std::slice::from_ref(&held),
        false,
        SystemTime::now(),
    )
    .unwrap();

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
    let report = sweep(
        &link,
        mtime_rule(DAY * 7),
        std::slice::from_ref(&held),
        false,
        SystemTime::now(),
    )
    .unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

    // Restore the mode before any assertion, so the tempdir can be removed.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    let locked_path = root.path().canonicalize().unwrap().join("locked");
    assert_eq!(report.failed, vec![locked_path.clone()]);
    // The doctor line names the cause, so the report must carry one for each failed path.
    let cause = report
        .causes
        .get(&locked_path)
        .expect("a cause for the failed entry");
    assert!(!cause.is_empty(), "cause: {cause:?}");
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

    let report = sweep(
        &tmp,
        mtime_rule(Duration::ZERO),
        &[],
        false,
        SystemTime::now(),
    )
    .unwrap();

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

    let summary =
        stale_summary_in(root.path(), mtime_rule(DAY * 7), &[], SystemTime::now()).unwrap();

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

    let summary = stale_summary_in(
        root.path(),
        mtime_rule(DAY * 7),
        std::slice::from_ref(&busy),
        SystemTime::now(),
    )
    .unwrap();

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

    let summary = stale_summary_in(&missing, mtime_rule(DAY * 7), &[], SystemTime::now()).unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

    assert!(report.removed.is_empty(), "{:?}", report.removed);
}

#[test]
fn a_fresh_folder_tree_with_no_file_is_kept() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("empty").join("sub")).unwrap();

    let report = sweep_week(root.path(), false).unwrap();

    assert!(report.removed.is_empty(), "{:?}", report.removed);
}

#[test]
fn an_old_folder_tree_with_no_file_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let entry = root.path().join("empty");
    fs::create_dir_all(entry.join("sub")).unwrap();
    age_tree(&entry, old());

    let report = sweep_week(root.path(), false).unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

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

    let report = sweep_week(root.path(), false).unwrap();

    assert!(report.removed.is_empty());
    assert!(hawk.exists());
}

// #3041: the in-use list is read only when the bound holds a stale or unreadable entry, and the
// shortcut never changes the answer.
proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
    #[test]
    fn the_shortcut_reads_lsof_only_when_needed_and_keeps_the_answer(
        entries in proptest::collection::vec((proptest::bool::ANY, proptest::bool::ANY), 0..4),
    ) {
        let state = tempfile::tempdir().unwrap();
        let tmp = state.path().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        let mut in_use = Vec::new();
        for (i, (is_old, busy)) in entries.iter().enumerate() {
            let entry = tmp.join(format!("e{i}"));
            fs::create_dir_all(&entry).unwrap();
            if *is_old {
                age_tree(&entry, SystemTime::now() - 2 * DAY);
            }
            if *busy {
                in_use.push(fs::canonicalize(&entry).unwrap());
            }
        }
        let calls = std::cell::Cell::new(0);
        let summary = stale_summary_with(state.path(), mtime_rule(DAY), || {
            calls.set(calls.get() + 1);
            Ok(in_use.clone())
        })
        .unwrap();
        let any_old = entries.iter().any(|(is_old, _)| *is_old);
        proptest::prop_assert_eq!(calls.get(), usize::from(any_old));
        let full = stale_summary_in(&tmp, mtime_rule(DAY), &in_use, SystemTime::now()).unwrap();
        proptest::prop_assert_eq!(summary.count, full.count);
        proptest::prop_assert_eq!(summary.unreadable, full.unreadable);
    }
}
