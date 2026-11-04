//! Tests for `llmenv sync`. The dry run runs `git status` in a real repository in a temp folder.

#![expect(
    clippy::expect_used,
    reason = "test code: a failed fixture stops the test"
)]

use super::{sync_dry_run_report, sync_in, sync_outcome_message};
use crate::sync::SyncOutcome;

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("run git init");
    assert!(status.success(), "git init must succeed");
    dir
}

#[test]
fn a_clean_tree_reports_nothing_to_sync() {
    let dir = repo();
    let report = sync_dry_run_report(dir.path()).expect("git status runs in a repository");
    assert_eq!(report, "Nothing to sync (working tree clean)");
}

#[test]
fn a_changed_file_is_listed_as_pending() {
    let dir = repo();
    std::fs::write(dir.path().join("settings.yaml"), "x").expect("write a file");
    let report = sync_dry_run_report(dir.path()).expect("git status runs in a repository");
    assert!(report.starts_with("Would sync:"), "{report}");
    assert!(report.contains("settings.yaml"), "{report}");
}

#[test]
fn a_folder_that_is_not_a_repository_fails_the_dry_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = sync_dry_run_report(dir.path());
    let message = match &result {
        Ok(_) => String::new(),
        Err(e) => format!("{e:#}"),
    };
    assert!(result.is_err(), "git status must fail outside a repository");
    assert!(message.contains("git status failed"), "{message}");
}

#[test]
fn each_outcome_has_its_own_line() {
    assert_eq!(
        sync_outcome_message(&SyncOutcome::NothingToCommit),
        "No changes to commit (working tree clean)"
    );
    assert_eq!(
        sync_outcome_message(&SyncOutcome::Pushed),
        "✓ Synced config to GitHub"
    );
    assert!(sync_outcome_message(&SyncOutcome::CommittedLocally).contains("push skipped"));
}

#[test]
fn a_dry_run_in_a_clean_repository_succeeds() {
    let dir = repo();
    sync_in(dir.path(), true).expect("a clean dry run is not an error");
}

#[test]
fn a_dry_run_outside_a_repository_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = sync_in(dir.path(), true);
    assert!(result.is_err(), "a dry run needs a repository to report on");
}
