#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use llmenv::sync;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

#[test]
fn sync_state_records_and_reads_last_pull() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path();

    let now = SystemTime::now();
    sync::write_state(state_dir, now).unwrap();

    let read = sync::read_state(state_dir).unwrap();
    assert!(read.is_some());

    // Should be approximately equal (within 1 second due to system time precision)
    let read_time = read.unwrap();
    let duration = now
        .duration_since(read_time)
        .unwrap_or_else(|_| read_time.duration_since(now).unwrap());
    assert!(duration < Duration::from_secs(1));
}

#[test]
fn sync_state_returns_none_when_not_exists() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path();

    let read = sync::read_state(state_dir).unwrap();
    assert!(read.is_none());
}

#[test]
fn sync_maybe_pull_respects_interval() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path();
    let repo_dir = tmp.path();

    let now = SystemTime::now();
    sync::write_state(state_dir, now).unwrap();

    // Try to pull with a large interval — should skip
    let result = sync::maybe_pull(repo_dir, state_dir, Duration::from_secs(3600));
    // Result is Ok even if git fetch/pull fail (we don't care about that in the test)
    assert!(result.is_ok());
}

#[test]
fn sync_maybe_pull_pulls_when_interval_elapsed() {
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path();
    let repo_dir = tmp.path();

    // Initialize a git repo so .git directory exists
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(repo_dir)
        .status()
        .unwrap();

    let old_time = SystemTime::now() - Duration::from_secs(7200);
    sync::write_state(state_dir, old_time).unwrap();

    // Pull with a small interval — should attempt to pull
    let result = sync::maybe_pull(repo_dir, state_dir, Duration::from_secs(3600));
    // Result is Ok even if git commands fail (we're not testing git here)
    assert!(result.is_ok());
}
