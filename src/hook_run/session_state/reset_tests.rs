use super::*;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn a_failed_read_state_reset_is_an_agent_notice() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new().expect("test");
    let cache = super::super::read_once::session_cache_path(dir.path(), "s1");
    std::fs::create_dir_all(cache.parent().expect("test")).expect("test");
    std::fs::write(&cache, "{}").expect("test");
    let folder = cache.parent().expect("test").to_path_buf();
    // A read-only folder refuses the unlink, so the read state survives the reset.
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).expect("test");

    reset_read_state(dir.path(), "s1");

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).expect("test");
    assert!(cache.exists());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains(&cache.display().to_string()) && notices.contains("Fix:"),
        "{notices}"
    );
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
fn a_compaction_reset_removes_the_session_read_state() {
    let dir = TempDir::new().expect("test");
    let own = super::super::read_once::session_cache_path(dir.path(), "s9");
    std::fs::create_dir_all(own.parent().expect("test")).expect("test");
    std::fs::write(&own, "{}").expect("test");

    reset_after_compaction_in(Some("s9"), Ok(dir.path().to_path_buf()));

    assert!(
        !own.exists(),
        "the compaction reset must remove the read state"
    );
}

#[test]
fn a_compaction_reset_without_a_state_dir_is_an_agent_notice() {
    reset_after_compaction_in(Some("s9"), Err(anyhow::anyhow!("no home marker 7f3a")));

    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("no home marker 7f3a") && notices.contains("Fix:"),
        "{notices}"
    );
}
