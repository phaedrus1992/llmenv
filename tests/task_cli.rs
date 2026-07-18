#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test scaffolding")]
//! Integration/smoke tests for `llmenv task` (#231).
//!
//! Drives the compiled binary end to end via `assert_cmd`, covering the full
//! CLI surface (add/start/done/ls/show/note/block), nesting via `--parent`,
//! prefix addressing, and error paths. Unit-level coverage (slug generation,
//! state transitions, identifier resolution edge cases, proptest invariants)
//! lives in `src/task/mod.rs`'s own test module — this file exercises the
//! CLI wiring on top of that, not the store logic itself.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use tempfile::TempDir;

fn llmenv(state_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_STATE_DIR", state_dir);
    cmd
}

#[test]
fn full_lifecycle_add_start_note_done() {
    let dir = TempDir::new().unwrap();

    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Added task"));

    let ls_json = llmenv(dir.path())
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    assert!(ls_json.status.success());
    let tasks: serde_json::Value = serde_json::from_slice(&ls_json.stdout).unwrap();
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["state"], "open");

    llmenv(dir.path())
        .args(["task", "start", "ship-the-release"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Started"));

    llmenv(dir.path())
        .args(["task", "note", "ship-the-release", "halfway there"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Noted"));

    let show_json = llmenv(dir.path())
        .args(["task", "show", "ship-the-release"])
        .output()
        .unwrap();
    assert!(show_json.status.success());
    let task: serde_json::Value = serde_json::from_slice(&show_json.stdout).unwrap();
    assert_eq!(task["state"], "wip");
    assert_eq!(task["notes"][0]["text"], "halfway there");

    llmenv(dir.path())
        .args(["task", "done", "ship-the-release"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Completed"));

    let final_ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&final_ls.stdout).unwrap();
    assert_eq!(tasks[0]["state"], "done");
}

#[test]
fn note_reads_from_stdin_when_text_omitted() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Piped note task"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "note", "piped-note-task"])
        .write_stdin("note via stdin")
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "piped-note-task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["notes"][0]["text"], "note via stdin");
}

#[test]
fn prefix_addressing_resolves_unambiguous_prefix() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Distinctive title here"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "start", "distinctive"])
        .assert()
        .success();
}

#[test]
fn ambiguous_prefix_fails_with_candidate_list() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Fix login timeout"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Fix logout crash"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "start", "fix-log"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("fix-login-timeout"))
        .stderr(predicates::str::contains("fix-logout-crash"));
}

#[test]
fn start_on_unknown_task_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "start", "no-such-task"])
        .assert()
        .failure();
}

#[test]
fn add_with_unknown_parent_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Orphan", "--parent", "no-such-parent"])
        .assert()
        .failure();
}

#[test]
fn block_on_unknown_target_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Lonely task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "block", "lonely-task", "--on", "ghost"])
        .assert()
        .failure();
}

// --- Nesting scenarios ---

#[test]
fn add_with_parent_links_child_via_cli() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Umbrella project"])
        .assert()
        .success();

    llmenv(dir.path())
        .args([
            "task",
            "add",
            "First subtask",
            "--parent",
            "umbrella-project",
        ])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "first-subtask"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["parent"], "umbrella-project");
}

#[test]
fn three_level_nesting_chain_via_cli() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Grandparent epic"])
        .assert()
        .success();
    llmenv(dir.path())
        .args([
            "task",
            "add",
            "Parent story",
            "--parent",
            "grandparent-epic",
        ])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child subtask", "--parent", "parent-story"])
        .assert()
        .success();

    let ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    let by_slug = |slug: &str| -> &serde_json::Value {
        tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["slug"] == slug)
            .expect("task must be present")
    };
    assert_eq!(
        by_slug("grandparent-epic")["parent"],
        serde_json::Value::Null
    );
    assert_eq!(by_slug("parent-story")["parent"], "grandparent-epic");
    assert_eq!(by_slug("child-subtask")["parent"], "parent-story");
}

#[test]
fn multiple_children_under_one_parent_via_cli() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Shared parent"])
        .assert()
        .success();
    for child_title in ["Child one", "Child two", "Child three"] {
        llmenv(dir.path())
            .args(["task", "add", child_title, "--parent", "shared-parent"])
            .assert()
            .success();
    }

    let ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    let children: Vec<&serde_json::Value> = tasks
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["parent"] == "shared-parent")
        .collect();
    assert_eq!(children.len(), 3);
}

#[test]
fn completing_child_does_not_change_parent_state_via_cli() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Parent task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child task", "--parent", "parent-task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "child-task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "child-task"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "parent-task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["state"], "open");
}

// --- New-project guard (Phase 3 CLI-side check) ---

#[test]
fn new_top_level_task_while_wip_exists_prints_guard_message() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "In progress work"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "in-progress-work"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "add", "Unrelated new thing"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress"));
}

#[test]
fn new_subtask_while_wip_exists_prints_no_guard_message() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "In progress work"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "in-progress-work"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "add", "Sub piece", "--parent", "in-progress-work"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress").not());
}
