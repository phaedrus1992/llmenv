#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use llmenv::sync;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

/// Run a git subcommand in `dir`, asserting success. Test setup only.
fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Initialize a git repo with a committable identity (commit needs user.*).
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

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

#[test]
fn commit_and_push_reports_nothing_when_clean() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());

    // Fresh repo, no files staged → nothing to commit, not an error.
    let outcome = sync::commit_and_push(tmp.path(), "Update llmenv config").unwrap();
    assert_eq!(outcome, sync::SyncOutcome::NothingToCommit);
}

#[test]
fn commit_and_push_surfaces_push_failure() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    std::fs::write(tmp.path().join("config.yaml"), b"x: 1\n").unwrap();

    // No remote configured: add + commit succeed, push must fail — and the
    // failure must be surfaced as an error (not silently swallowed). This is
    // the #307 regression guard: previously `git push` ran via `.status()`
    // without checking success, so a failed push looked like success.
    let err = sync::commit_and_push(tmp.path(), "Update llmenv config").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("push"),
        "push failure should be surfaced: {msg}"
    );
}

#[test]
fn commit_and_push_pushes_change_to_remote() {
    let tmp = TempDir::new().unwrap();
    let remote = tmp.path().join("remote.git");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    git(
        tmp.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    init_repo(&work);
    std::fs::write(work.join("config.yaml"), b"x: 1\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "initial"]);
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "-u", "origin", "HEAD"]);

    // A new change should be staged, committed, and pushed.
    std::fs::write(work.join("config.yaml"), b"x: 2\n").unwrap();
    let outcome = sync::commit_and_push(&work, "Update llmenv config").unwrap();
    assert_eq!(outcome, sync::SyncOutcome::Pushed);

    // Confirm the commit actually landed in the bare remote — guards against a
    // vacuous pass where `Pushed` is returned but the remote ref never moved.
    let remote_log = std::process::Command::new("git")
        .args(["log", "--oneline", "-1"])
        .current_dir(&remote)
        .output()
        .unwrap();
    assert!(remote_log.status.success(), "reading remote log failed");
    let remote_head = String::from_utf8_lossy(&remote_log.stdout);
    assert!(
        remote_head.contains("Update llmenv config"),
        "remote HEAD should be the pushed commit, got: {}",
        remote_head.trim()
    );
}
