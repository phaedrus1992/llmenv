//! Tests of the stale summary, and of the age source on the sweep (#3195).

use std::fs;
use std::time::SystemTime;

use super::tests::{DAY, age_tree, mtime_rule, old};
use super::{StaleSummary, stale_summary_in, stale_summary_with, sweep};
use crate::materialize::cache::AgeSource;
use crate::materialize::expiry::AgeRule;

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
    let report = sweep(
        root.path(),
        mtime_rule(DAY * 7),
        &[],
        false,
        SystemTime::now(),
    )
    .unwrap();
    assert_eq!(report.removed, vec![unpacked.clone()]);
    assert!(!unpacked.exists());
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
