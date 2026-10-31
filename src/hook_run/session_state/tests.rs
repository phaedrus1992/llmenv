use super::*;

#[test]
fn the_agent_key_hashes_the_session_and_the_agent_id_as_text() {
    let key = agent_state_key("s1", &serde_json::json!({ "agent_id": "a7" }));
    let hash = super::super::record_hash::hash_prefix(b"s1\0a7");
    assert_eq!(key, format!("s1.agent-{hash}"));
}
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

#[test]
fn prunes_only_stale_json_files() {
    let dir = TempDir::new().expect("test");
    let fresh = dir.path().join("fresh.json");
    let stale = dir.path().join("stale.json");
    let non_json = dir.path().join("ignored.txt");
    std::fs::write(&fresh, "{}").expect("test");
    std::fs::write(&stale, "{}").expect("test");
    std::fs::write(&non_json, "not json").expect("test");

    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    let old_ft = filetime::FileTime::from_system_time(old);
    filetime::set_file_mtime(&stale, old_ft).expect("test");

    prune_stale_json_files(dir.path(), 7);

    assert!(fresh.exists(), "fresh file must survive pruning");
    assert!(!stale.exists(), "stale file must be pruned");
    assert!(non_json.exists(), "non-json file must never be touched");
}

#[test]
fn file_age_is_none_for_a_missing_file_and_an_error_for_an_unreadable_path() {
    let dir = TempDir::new().expect("test");
    assert!(file_age(&dir.path().join("absent.json")).unwrap().is_none());
    let file = dir.path().join("file");
    std::fs::write(&file, "x").expect("test");
    // A path under a regular file fails with ENOTDIR, not NotFound.
    assert!(file_age(&file.join("child.lock")).is_err());
    assert!(file_age(&file).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn a_lock_whose_age_cannot_be_read_is_kept() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new().expect("test");
    let locks = dir.path().join("locks");
    std::fs::create_dir(&locks).expect("test");
    let lock = locks.join("old.lock");
    std::fs::write(&lock, "").expect("test");
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&lock, filetime::FileTime::from_system_time(old)).expect("test");
    // Read without search permission: the folder lists, but a stat of its entries fails.
    std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o444)).expect("test");

    let logs = crate::test_log_capture::capture_logs(|| prune_orphan_locks(&locks, 7));

    std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o755)).expect("test");
    assert!(lock.exists(), "a lock whose age cannot be read must stay");
    assert!(
        logs.contains("old.lock") && logs.contains("stays on disk"),
        "the cause must be logged: {logs}"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_prune_is_an_agent_notice() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new().expect("test");
    let stale = dir.path().join("stale.json");
    std::fs::write(&stale, "{}").expect("test");
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old)).expect("test");
    // A read-only folder refuses the unlink.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).expect("test");

    prune_stale_json_files(dir.path(), 7);

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("test");
    assert!(stale.exists());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains(&stale.display().to_string()) && notices.contains("Fix:"),
        "{notices}"
    );
}

#[cfg(unix)]
#[test]
fn a_fresh_link_to_an_old_file_is_kept() {
    let dir = TempDir::new().expect("test");
    // The target lives outside the scanned folder, so only the link is judged there.
    let elsewhere = TempDir::new().expect("test");
    let target = elsewhere.path().join("target.json");
    std::fs::write(&target, "{}").expect("test");
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(old)).expect("test");
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&target, &link).expect("test");

    prune_stale_json_files(dir.path(), 7);

    assert!(
        link.is_symlink(),
        "a fresh link must be judged by its own age"
    );
    assert!(target.exists(), "the link target must not be touched");
}

#[cfg(unix)]
#[test]
fn an_old_link_to_a_fresh_file_is_pruned_as_a_link() {
    let dir = TempDir::new().expect("test");
    let target = dir.path().join("target.json");
    std::fs::write(&target, "{}").expect("test");
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&target, &link).expect("test");
    let old =
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(10 * 86_400));
    filetime::set_symlink_file_times(&link, old, old).expect("test");

    prune_stale_json_files(dir.path(), 7);

    assert!(
        !link.is_symlink(),
        "an old link must be removed on its own age"
    );
    assert!(target.exists(), "the link target must not be removed");
}

#[cfg(unix)]
#[test]
fn a_fresh_lock_link_to_an_old_lock_is_kept() {
    let dir = TempDir::new().expect("test");
    let target = dir.path().join("old.lock");
    std::fs::write(&target, "").expect("test");
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(old)).expect("test");
    let link = dir.path().join("link.lock");
    std::os::unix::fs::symlink(&target, &link).expect("test");

    prune_orphan_locks(dir.path(), 7);

    assert!(
        link.is_symlink(),
        "a fresh lock link must be judged by its own age"
    );
}

#[cfg(unix)]
#[test]
fn an_old_lock_link_to_a_fresh_lock_is_pruned_as_a_link() {
    let dir = TempDir::new().expect("test");
    let target = dir.path().join("fresh.lock");
    std::fs::write(&target, "").expect("test");
    let link = dir.path().join("link.lock");
    std::os::unix::fs::symlink(&target, &link).expect("test");
    let old =
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(10 * 86_400));
    filetime::set_symlink_file_times(&link, old, old).expect("test");

    prune_orphan_locks(dir.path(), 7);

    assert!(
        !link.is_symlink(),
        "an old lock link must be removed on its own age"
    );
    assert!(target.exists(), "the link target must not be removed");
}

#[test]
fn a_file_at_the_limit_is_kept() {
    let day = Duration::from_secs(86_400);
    assert!(!is_stale(day, day));
    assert!(is_stale(day + Duration::from_secs(1), day));
    assert!(!is_stale(Duration::ZERO, day));
}

#[test]
fn missing_dir_is_a_noop() {
    let dir = TempDir::new().expect("test");
    prune_stale_json_files(&dir.path().join("does-not-exist"), 7);
}

#[test]
fn reset_read_state_removes_both_files_of_one_session_only() {
    let dir = TempDir::new().expect("test");
    let own = [
        super::super::read_once::session_cache_path(dir.path(), "s1"),
        super::super::slippage::stats_path(dir.path(), "s1"),
        super::super::repeat_detect::session_state_path(dir.path(), "s1"),
    ];
    let other = super::super::read_once::session_cache_path(dir.path(), "s2");
    for p in own.iter().chain([&other]) {
        std::fs::create_dir_all(p.parent().expect("test")).expect("test");
        std::fs::write(p, "{}").expect("test");
    }

    reset_read_state(dir.path(), "s1");

    assert!(own.iter().all(|p| !p.exists()), "own state must go");
    assert!(other.exists(), "another session's state must stay");
    reset_read_state(dir.path(), "s1");
    reset_read_state(dir.path(), "../escape");
}

#[test]
fn remove_if_present_accepts_a_missing_file_and_reports_a_real_error() {
    let dir = TempDir::new().expect("test");
    let file = dir.path().join("f.json");
    remove_if_present(&file).unwrap();
    std::fs::write(&file, "{}").expect("test");
    remove_if_present(&file).unwrap();
    assert!(!file.exists());
    // `remove_file` fails on a directory, which is not a NotFound error.
    let sub = dir.path().join("d");
    std::fs::create_dir(&sub).expect("test");
    assert!(remove_if_present(&sub).is_err());
}

#[test]
fn only_context_losing_sources_reset() {
    let got: Vec<bool> = ["startup", "clear", "compact", "resume", "fork"]
        .iter()
        .map(|s| context_was_lost(Some(s)))
        .collect();
    assert_eq!(got, [true, true, true, false, false]);
    assert!(!context_was_lost(None));
}

#[test]
fn unix_now_reads_the_real_clock() {
    let expected = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test")
        .as_secs();
    let now = u64::try_from(unix_now()).expect("test");
    assert!(now.abs_diff(expected) <= 5, "{now} vs {expected}");
}

// #3116: an entry that exists only as a dangling link still belongs to its session, so the
// lock beside it is not an orphan. `exists()` follows the link and reports the entry as absent.
#[cfg(unix)]
#[test]
fn a_lock_stays_when_its_json_is_a_dangling_link() {
    let dir = TempDir::new().expect("test");
    let lock = dir.path().join("s.lock");
    std::fs::write(&lock, "").expect("test");
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&lock, filetime::FileTime::from_system_time(old)).expect("test");
    std::os::unix::fs::symlink(dir.path().join("gone.json"), dir.path().join("s.json"))
        .expect("test");

    prune_orphan_locks(dir.path(), 7);

    assert!(
        lock.exists(),
        "a lock whose .json entry is a dangling link must stay"
    );
}

// A lookup that fails for a reason other than "not found" must keep the lock. Here the parent is a
// regular file, so the lookup fails with ENOTDIR.
#[cfg(unix)]
#[test]
fn an_unreadable_session_entry_counts_as_present() {
    let dir = TempDir::new().expect("test");
    let file = dir.path().join("regular");
    std::fs::write(&file, "x").expect("test");

    assert!(!json_entry_is_absent(&file.join("s.json")));
}
