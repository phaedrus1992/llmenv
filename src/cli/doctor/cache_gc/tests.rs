use std::fs::File;
use std::time::{Duration, SystemTime};

use super::cache_sweep::{sweep_cache, sweep_cache_root};
use super::{GcOutput, sweep_state};
use crate::config::Config;

const DAY: Duration = Duration::from_secs(86_400);
const OUT: GcOutput<'static> = GcOutput {
    pass: "ok",
    warn: "warn",
    dry_run: false,
};

#[test]
fn a_configured_index_path_marks_the_folder_live() {
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("codebase-memory");
    std::fs::create_dir(&dir).unwrap();
    let with = |index_path: &str| -> Config {
        serde_yaml::from_str(&format!(
            "features:\n  codebase_memory:\n    - index_path: {index_path}\n"
        ))
        .unwrap()
    };
    use super::{CacheUse, cache_use};
    assert_eq!(
        cache_use(&with(&dir.display().to_string()), state.path(), &dir),
        CacheUse::Live
    );
    assert_eq!(
        cache_use(&with("/elsewhere/cbm"), state.path(), &dir),
        CacheUse::Unused
    );
    assert_eq!(
        cache_use(&Config::default(), state.path(), &dir),
        CacheUse::Unused
    );
}

#[test]
fn a_bundle_index_path_marks_the_folder_live() {
    use super::{CacheUse, cache_use};
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("codebase-memory");
    std::fs::create_dir(&dir).unwrap();
    let bundles = state.path().join("bundles");
    std::fs::create_dir_all(bundles.join("work")).unwrap();
    std::fs::write(
        bundles.join("work/bundle.yaml"),
        format!(
            "features:\n  codebase_memory:\n    - when: [work]\n      index_path: {}\n",
            dir.display()
        ),
    )
    .unwrap();
    let config: Config = serde_yaml::from_str("bundle:\n  - name: work\n").unwrap();

    assert_eq!(cache_use(&config, &bundles, &dir), CacheUse::Live);
}

#[test]
fn an_unreadable_bundle_leaves_the_folder_use_unknown() {
    use super::{CacheUse, cache_use};
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("codebase-memory");
    std::fs::create_dir(&dir).unwrap();
    let bundles = state.path().join("bundles");
    std::fs::create_dir_all(bundles.join("work")).unwrap();
    std::fs::write(
        bundles.join("work/bundle.yaml"),
        "features: [not, a, mapping]\n",
    )
    .unwrap();
    let config: Config = serde_yaml::from_str("bundle:\n  - name: work\n").unwrap();

    assert!(matches!(
        cache_use(&config, &bundles, &dir),
        CacheUse::Unknown(_)
    ));
}

#[test]
// A test cannot set a change time, so removal by age is covered by the prune tests, which use
// the mtime only. This test checks the change-time rule: a rotation restored with an old mtime
// has a fresh change time, so it stays.
fn the_state_sweep_keeps_a_rotation_restored_with_an_old_mtime() {
    let state = tempfile::tempdir().unwrap();
    let active = state.path().join("session-log.jsonl");
    std::fs::write(&active, b"{}").unwrap();
    let rotated = state.path().join("session-log.jsonl.1");
    std::fs::write(&rotated, b"{}").unwrap();
    File::options()
        .write(true)
        .open(&rotated)
        .unwrap()
        .set_modified(SystemTime::now() - 30 * DAY)
        .unwrap();

    sweep_state(
        state.path(),
        &Config::default(),
        DAY * 7,
        SystemTime::now(),
        OUT,
    );

    assert!(rotated.exists(), "a restored rotation was removed");
    assert!(active.exists(), "the active log was removed");
}

// #3335: a test cannot set a change time on a real file. A later clock makes a rotation whose
// mtime and change time are both new count as old, so the removal runs through the same sweep.
#[test]
fn the_state_sweep_removes_a_rotation_old_by_mtime_and_change_time() {
    let state = tempfile::tempdir().unwrap();
    let rotated = state.path().join("session-log.jsonl.1");
    std::fs::write(&rotated, b"{}").unwrap();
    let later = SystemTime::now() + 30 * DAY;

    sweep_state(state.path(), &Config::default(), DAY * 7, later, OUT);

    assert!(!rotated.exists(), "an old rotation was kept");
}

#[test]
fn a_dry_run_of_the_state_sweep_keeps_an_old_rotation() {
    let state = tempfile::tempdir().unwrap();
    let rotated = state.path().join("session-log.jsonl.1");
    std::fs::write(&rotated, b"{}").unwrap();
    File::options()
        .write(true)
        .open(&rotated)
        .unwrap()
        .set_modified(SystemTime::now() - 30 * DAY)
        .unwrap();

    let preview = GcOutput {
        dry_run: true,
        ..OUT
    };
    sweep_state(
        state.path(),
        &Config::default(),
        DAY * 7,
        SystemTime::now(),
        preview,
    );

    assert!(rotated.exists(), "a dry run removed an old rotation");
}

#[test]
fn the_state_sweep_with_no_tmp_folder_and_no_log_changes_nothing() {
    let state = tempfile::tempdir().unwrap();
    sweep_state(
        state.path(),
        &Config::default(),
        DAY * 7,
        SystemTime::now(),
        OUT,
    );
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}

#[test]
fn the_cache_sweep_keeps_a_fresh_entry() {
    let cache = tempfile::tempdir().unwrap();
    let entry = cache.path().join("fresh");
    std::fs::create_dir(&entry).unwrap();
    let failures = sweep_cache(cache.path(), &Config::default(), DAY * 7, OUT);
    assert!(entry.exists(), "a fresh cache entry was removed");
    assert_eq!(failures, 0);
}

#[test]
fn a_symlink_cache_root_is_refused_and_a_dangling_one_is_too() {
    let base = tempfile::tempdir().unwrap();
    let target = base.path().join("real");
    std::fs::create_dir(&target).unwrap();
    let live = base.path().join("live-link");
    std::os::unix::fs::symlink(&target, &live).unwrap();
    let dangling = base.path().join("dangling-link");
    std::os::unix::fs::symlink(base.path().join("absent"), &dangling).unwrap();
    for link in [&live, &dangling] {
        let failures = sweep_cache_root(link, &Config::default(), DAY * 7, OUT);
        assert_eq!(
            failures, 1,
            "a symlink root must count as a failure: {link:?}"
        );
    }
}

#[test]
fn the_cache_sweep_reports_an_unreadable_cache_and_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_dir = dir.path().join("file");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let failures = sweep_cache(&not_a_dir, &Config::default(), DAY * 7, OUT);
    assert!(not_a_dir.exists());
    assert_eq!(
        failures, 1,
        "an unreadable cache must count as a failed sweep"
    );
}
