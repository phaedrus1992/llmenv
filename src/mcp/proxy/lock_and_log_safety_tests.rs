use super::{lock_holder, open_bounded_log, try_lock};
use tempfile::TempDir;

#[test]
fn try_lock_records_the_pid_before_the_lock_is_visible() {
    let dir = TempDir::new().expect("tempdir");
    let lock = dir.path().join("proxy.lock");

    assert!(try_lock(&lock).expect("first take").is_some());
    let pid = std::process::id().to_string();
    assert_eq!(std::fs::read_to_string(&lock).expect("read lock"), pid);

    assert!(
        try_lock(&lock).expect("second take").is_none(),
        "a held lock must not be taken again"
    );
    assert_eq!(lock_holder(&lock).expect("holder"), pid.parse::<u32>().ok());
}

#[test]
fn lock_holder_treats_an_absent_lock_as_no_holder() {
    let dir = TempDir::new().expect("tempdir");
    let lock = dir.path().join("absent.lock");

    assert_eq!(lock_holder(&lock).expect("absent is not an error"), None);
}

#[test]
fn lock_holder_fails_closed_on_a_read_error_other_than_absent() {
    let dir = TempDir::new().expect("tempdir");
    // A directory at the lock path reads as an error that is not NotFound.
    let lock = dir.path().join("is-a-dir.lock");
    std::fs::create_dir(&lock).expect("mkdir");

    assert!(
        lock_holder(&lock).is_err(),
        "an unreadable lock must not read as stale"
    );
}

#[cfg(unix)]
#[test]
fn open_bounded_log_rotates_at_the_limit_and_not_below_it() {
    let dir = TempDir::new().expect("tempdir");
    let log = dir.path().join("proxy.log");
    std::fs::write(&log, vec![b'x'; 1023]).expect("seed below limit");
    drop(open_bounded_log(&log, 1024, false).expect("open below limit"));
    assert!(
        !dir.path().join("proxy.log.1").exists(),
        "a log below the limit must not rotate"
    );
    std::fs::write(&log, vec![b'x'; 1024]).expect("seed at limit");
    drop(open_bounded_log(&log, 1024, false).expect("open at limit"));
    assert!(
        dir.path().join("proxy.log.1").exists(),
        "a log at the limit must rotate"
    );
}

#[cfg(unix)]
#[test]
fn open_bounded_log_refuses_a_symlink_at_the_log_path() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path().join("target.log");
    std::fs::write(&target, b"").expect("target");
    let log = dir.path().join("proxy.log");
    std::os::unix::fs::symlink(&target, &log).expect("symlink");

    assert!(
        open_bounded_log(&log, 1024, false).is_err(),
        "a symlinked log must be refused, not written through"
    );
    assert_eq!(std::fs::read(&target).expect("read target"), b"");
}

#[cfg(unix)]
#[test]
fn write_pidfile_atomic_replaces_a_symlink_at_the_temp_name() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_path = tmp.path().join("mcp-proxy.pid");
    let temp_name = pid_path.with_extension(format!("pid.{}.tmp", std::process::id()));
    let victim = tmp.path().join("victim");
    std::fs::write(&victim, "secret").unwrap();
    std::os::unix::fs::symlink(&victim, &temp_name).unwrap();

    crate::mcp::proxy::write_pidfile_atomic(&pid_path, 4242).unwrap();

    assert_eq!(std::fs::read_to_string(&pid_path).unwrap(), "4242");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "secret");
}

#[cfg(unix)]
#[test]
fn open_bounded_log_refuses_a_symlinked_parent_when_not_hardened() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = open_bounded_log(&link.join("proxy.log"), 1024, false)
        .expect_err("a symlinked parent must be refused when not hardened");
    assert!(err.to_string().contains("is a symlink"), "{err}");
}

#[cfg(unix)]
#[test]
fn open_bounded_log_refuses_a_hard_linked_file() {
    let dir = TempDir::new().expect("tempdir");
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"keep").expect("victim");
    let log = dir.path().join("proxy.log");
    std::fs::hard_link(&victim, &log).expect("hard link");

    assert!(
        open_bounded_log(&log, 1024, false).is_err(),
        "a log with another hard link must be refused"
    );
    assert_eq!(std::fs::read(&victim).expect("read"), b"keep");
}

#[test]
fn staging_name_is_not_the_pidfile_temp_name() {
    let lock = std::path::Path::new("/state/mcp-proxy.pid.lock");

    let staging = super::lockfile::staging_path(lock);

    let name = staging.to_string_lossy().into_owned();
    assert!(name.starts_with("/state/mcp-proxy.pid.lock."), "{name}");
    assert!(name.ends_with(".staging"), "{name}");
}

proptest::proptest! {
    #[test]
    fn lock_holder_never_errors_on_file_content(content in ".*") {
        let dir = TempDir::new().expect("tempdir");
        let lock = dir.path().join("p.lock");
        std::fs::write(&lock, content).expect("write");

        proptest::prop_assert!(lock_holder(&lock).is_ok());
    }

    #[test]
    fn lock_holder_reads_back_any_pid(pid in proptest::prelude::any::<u32>(), pad in "[ \\t\\n]{0,3}") {
        let dir = TempDir::new().expect("tempdir");
        let lock = dir.path().join("p.lock");
        std::fs::write(&lock, format!("{pad}{pid}{pad}")).expect("write");

        proptest::prop_assert_eq!(lock_holder(&lock).expect("holder"), Some(pid));
    }
}

/// A lock whose pid cannot be read is reclaimed, and the reclaim is named for the agent. The
/// spawn fails on purpose, so the test stops after the reclaim (#3217).
#[test]
fn reclaimed_unreadable_lock_is_recorded_for_the_agent() {
    use super::adopt_or_reclaim;
    let dir = TempDir::new().expect("tempdir");
    let lock = dir.path().join("proxy-3217-garbage.lock");
    std::fs::write(&lock, "not-a-pid").expect("write lock");
    let pid_path = dir.path().join("mcp-proxy.pid");
    let result = adopt_or_reclaim(
        "127.0.0.1:1",
        &pid_path,
        |_| anyhow::bail!("spawn is not part of this test"),
        50,
        &lock,
    );
    assert!(result.is_err(), "the failed spawn must surface");
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("proxy-3217-garbage.lock"), "{notices}");
    assert!(notices.contains("Fix:"), "{notices}");
}
