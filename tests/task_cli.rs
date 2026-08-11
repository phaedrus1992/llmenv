#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test scaffolding")]
//! Integration/smoke tests for `llmenv task` (#231, reworked for mandatory
//! sessions — docs/superpowers/specs/2026-07-21-task-project-scoping-design.md).
//!
//! Drives the compiled binary end to end via `assert_cmd`, covering the full
//! CLI surface (add/start/done/ls/show/note/wait/block/clear + session
//! start/finish/show/ls), nesting via `--parent`, prefix addressing, the
//! mandatory-session enforcement, and the resume/replace/new checkpoint.
//! Unit-level coverage (slug generation, state transitions, session store
//! logic, proptest invariants) lives in `src/task/`'s own test modules.
//!
//! Every test runs with cwd = the repo root (assert_cmd's default), so the
//! resolved project tag is the same for all `llmenv task` calls within a
//! test; sessions are isolated per test via a temp `LLMENV_STATE_DIR`.

use predicates::prelude::PredicateBooleanExt;
use tempfile::TempDir;

mod support;
use support::isolated_llmenv_cmd as llmenv;

/// Start a session so subsequent `task add` calls auto-resolve to it.
fn start_session(dir: &std::path::Path, name: &str) {
    llmenv(dir)
        .args(["task", "session", "start", name])
        .assert()
        .success();
}

#[test]
fn full_lifecycle_add_start_note_done() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");

    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Added task"));

    let ls_json = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--all"])
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
        .args(["task", "ls", "--format", "json", "--all"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&final_ls.stdout).unwrap();
    assert_eq!(tasks[0]["state"], "done");
}

#[test]
fn note_reads_from_stdin_when_text_omitted() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
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
    start_session(dir.path(), "sprint");
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
    start_session(dir.path(), "sprint");
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
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Orphan", "--parent", "no-such-parent"])
        .assert()
        .failure();
}

#[test]
fn block_on_unknown_target_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Lonely task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "block", "lonely-task", "--on", "ghost"])
        .assert()
        .failure();
}

// --- Edit (#930) ---

#[test]
fn edit_retitles_a_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Original title", "--no-parent"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "edit", "original-title", "--title", "New title"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Updated"));

    let show = llmenv(dir.path())
        .args(["task", "show", "original-title"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["title"], "New title");
}

#[test]
fn edit_sets_and_clears_parent() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Parent", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child", "--no-parent"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "edit", "child", "--parent", "parent"])
        .assert()
        .success();
    let show = llmenv(dir.path())
        .args(["task", "show", "child"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["parent"], "parent");

    llmenv(dir.path())
        .args(["task", "edit", "child", "--no-parent"])
        .assert()
        .success();
    let show = llmenv(dir.path())
        .args(["task", "show", "child"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["parent"], serde_json::Value::Null);
}

#[test]
fn edit_parent_and_no_parent_flags_conflict() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Solo", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "edit", "solo", "--parent", "solo", "--no-parent"])
        .assert()
        .failure();
}

#[test]
fn edit_parent_to_unknown_id_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Solo", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "edit", "solo", "--parent", "no-such-task"])
        .assert()
        .failure();
}

#[test]
fn edit_parent_creating_a_cycle_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "A", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "B", "--parent", "a"])
        .assert()
        .success();

    // A -> B already; making A's parent B would make A its own ancestor.
    llmenv(dir.path())
        .args(["task", "edit", "a", "--parent", "b"])
        .assert()
        .failure();
}

#[test]
fn edit_adds_and_removes_blocked_on() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Blocker", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Blocked", "--no-parent"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "edit", "blocked", "--block-on", "blocker"])
        .assert()
        .success();
    let show = llmenv(dir.path())
        .args(["task", "show", "blocked"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["blocked_on"][0], "blocker");

    llmenv(dir.path())
        .args(["task", "edit", "blocked", "--unblock", "blocker"])
        .assert()
        .success();
    let show = llmenv(dir.path())
        .args(["task", "show", "blocked"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert!(task["blocked_on"].as_array().unwrap().is_empty());
}

#[test]
fn edit_block_on_self_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Solo", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "edit", "solo", "--block-on", "solo"])
        .assert()
        .failure();
}

#[test]
fn edit_add_note_appends_and_delete_note_removes_by_index() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task", "--no-parent"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "edit", "task", "--add-note", "first note"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "edit", "task", "--add-note", "second note"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["notes"][0]["text"], "first note");
    assert_eq!(task["notes"][1]["text"], "second note");

    llmenv(dir.path())
        .args(["task", "edit", "task", "--delete-note", "0"])
        .assert()
        .success();
    let show = llmenv(dir.path())
        .args(["task", "show", "task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["notes"].as_array().unwrap().len(), 1);
    assert_eq!(task["notes"][0]["text"], "second note");
}

#[test]
fn edit_add_note_reads_from_stdin_when_given_empty() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task", "--no-parent"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "edit", "task", "--add-note", ""])
        .write_stdin("note via stdin")
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["notes"][0]["text"], "note via stdin");
}

#[test]
fn edit_delete_note_by_timestamp() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "note", "task", "only note"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    let at = task["notes"][0]["at"].as_str().unwrap().to_string();

    llmenv(dir.path())
        .args(["task", "edit", "task", "--delete-note", &at])
        .assert()
        .success();
    let show = llmenv(dir.path())
        .args(["task", "show", "task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert!(task["notes"].as_array().unwrap().is_empty());
}

#[test]
fn edit_delete_note_out_of_range_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "edit", "task", "--delete-note", "0"])
        .assert()
        .failure();
}

#[test]
fn edit_with_no_flags_is_a_noop_that_succeeds() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task", "--no-parent"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "edit", "task"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["title"], "Task");
    assert_eq!(task["parent"], serde_json::Value::Null);
}

#[test]
fn edit_on_unknown_task_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "edit", "no-such-task", "--title", "New"])
        .assert()
        .failure();
}

// --- Nesting scenarios ---

#[test]
fn add_with_parent_links_child_via_cli() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
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
    start_session(dir.path(), "sprint");
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
        .args(["task", "ls", "--format", "json", "--all"])
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

// --- Implicit parent chaining (#929) ---

#[test]
fn bare_add_chains_onto_the_previously_added_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    for title in ["First", "Second", "Third"] {
        llmenv(dir.path())
            .args(["task", "add", title])
            .assert()
            .success();
    }

    let ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--all"])
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
    assert_eq!(by_slug("first")["parent"], serde_json::Value::Null);
    assert_eq!(by_slug("second")["parent"], "first");
    assert_eq!(by_slug("third")["parent"], "second");
}

#[test]
fn explicit_parent_overrides_the_implicit_chain() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "First"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Second"])
        .assert()
        .success();
    // Bypass the implicit chain and nest explicitly under "First" instead.
    llmenv(dir.path())
        .args(["task", "add", "Third", "--parent", "first"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "third"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["parent"], "first");
}

#[test]
fn no_parent_flag_forces_a_top_level_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "First"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Deliberately unrelated", "--no-parent"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "deliberately-unrelated"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["parent"], serde_json::Value::Null);
}

#[test]
fn parent_and_no_parent_flags_conflict() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "First"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Second", "--parent", "first", "--no-parent"])
        .assert()
        .failure();
}

#[test]
fn first_task_in_a_new_session_has_no_implicit_parent() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Very first task"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "very-first-task"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["parent"], serde_json::Value::Null);
}

#[test]
fn implicit_chaining_never_crosses_sessions() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint-1");
    llmenv(dir.path())
        .args(["task", "add", "In sprint one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "finish"])
        .assert()
        .success();

    start_session(dir.path(), "sprint-2");
    llmenv(dir.path())
        .args(["task", "add", "In sprint two"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "in-sprint-two"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(
        task["parent"],
        serde_json::Value::Null,
        "a new session's first task must not chain onto a prior session's last task"
    );
}

#[test]
fn multiple_children_under_one_parent_via_cli() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
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
        .args(["task", "ls", "--format", "json", "--all"])
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
    start_session(dir.path(), "sprint");
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
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "In progress work"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "in-progress-work"])
        .assert()
        .success();

    // #929: a bare `task add` (no --parent) now defaults to chaining onto
    // the previous task, so it's no longer "unrelated" — only an explicit
    // `--no-parent` is still the deliberate top-level case the guard warns
    // about.
    llmenv(dir.path())
        .args(["task", "add", "Unrelated new thing", "--no-parent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress"));
}

#[test]
fn implicit_chain_while_wip_exists_prints_no_guard_message() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "In progress work"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "in-progress-work"])
        .assert()
        .success();

    // A bare `task add` chains onto "In progress work" by default (#929) —
    // no longer the guard's "unrelated top-level task" case.
    llmenv(dir.path())
        .args(["task", "add", "Chained follow-up"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress").not());
}

#[test]
fn new_subtask_while_wip_exists_prints_no_guard_message() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
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

// --- Mandatory sessions (2026-07-21 rework) ---

#[test]
fn task_add_without_a_session_errors() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Orphan task"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("session start"));
}

#[test]
fn session_start_then_task_add_auto_resolves() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args([
            "task",
            "session",
            "start",
            "sprint",
            "--description",
            "issue 493",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Started session"));
    llmenv(dir.path())
        .args(["task", "add", "Ship it"])
        .assert()
        .success();
    let ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--all"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert!(tasks[0]["session"].is_string());
}

#[test]
fn session_start_twice_without_a_flag_errors_listing_the_existing_one() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "first");
    llmenv(dir.path())
        .args(["task", "session", "start", "second"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("first"))
        .stderr(predicates::str::contains("--resume"));
}

#[test]
fn session_start_resume_adopts_the_existing_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "first");
    llmenv(dir.path())
        .args(["task", "session", "start", "--resume", "first"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Resumed session"));
    // Still exactly one open session (no new id created).
    let ls = llmenv(dir.path())
        .args(["task", "session", "ls"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(ls.stdout).unwrap();
    assert_eq!(stdout.lines().filter(|l| l.starts_with("first")).count(), 1);
}

#[test]
fn session_start_replace_abandons_and_creates_fresh() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "first");
    llmenv(dir.path())
        .args(["task", "add", "Never finished"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "session", "start", "second", "--replace"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Abandoned session"))
        .stdout(predicates::str::contains("Started session"));

    // The orphaned task is untagged and notes what happened, but still exists.
    let show = llmenv(dir.path())
        .args(["task", "show", "never-finished"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["session"], serde_json::Value::Null);
    assert!(
        task["notes"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Orphaned")
    );
}

#[test]
fn session_start_new_allows_concurrent_sessions_in_the_same_project() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "first");
    llmenv(dir.path())
        .args(["task", "session", "start", "second", "--new"])
        .assert()
        .success();
    let ls = llmenv(dir.path())
        .args(["task", "session", "ls"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(ls.stdout).unwrap();
    assert!(stdout.contains("first"));
    assert!(stdout.contains("second"));
    // §5: `session ls` shows an idle duration per session.
    assert!(
        stdout.contains("idle "),
        "session ls must show idle duration: {stdout}"
    );
}

#[test]
fn task_add_with_two_open_sessions_requires_explicit_session_flag() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "first");
    llmenv(dir.path())
        .args(["task", "session", "start", "second", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Ambiguous"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--session"));

    // Explicit --session resolves the ambiguity.
    llmenv(dir.path())
        .args(["task", "add", "Explicit", "--session", "second"])
        .assert()
        .success();
}

#[test]
fn session_finish_by_id_closes_it_out() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "session", "finish", "sprint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Finished session"));
    llmenv(dir.path())
        .args(["task", "session", "ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No open sessions"));
}

#[test]
fn session_finish_auto_resolves_when_exactly_one_open() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "only");
    llmenv(dir.path())
        .args(["task", "session", "finish"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Finished session"));
}

#[test]
fn session_show_unknown_id_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "show", "no-such-session"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no session"));
}

#[test]
fn session_finish_with_no_open_session_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "finish"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no open session"));
}

#[test]
fn session_show_reports_progress() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "show"])
        .assert()
        .success()
        .stdout(predicates::str::contains("0/1 done"));
    llmenv(dir.path())
        .args(["task", "done", "ship-the-release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "show"])
        .assert()
        .success()
        .stdout(predicates::str::contains("1/1 done"));
}

// --- Session summary (#931) ---

#[test]
fn session_summary_json_includes_tasks_and_notes() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "note", "ship-the-release", "made progress"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "ship-the-release"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "session", "summary", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["name"], "sprint");
    assert_eq!(summary["done"], 1);
    assert_eq!(summary["total"], 1);
    assert_eq!(summary["tasks"][0]["slug"], "ship-the-release");
    assert_eq!(summary["tasks"][0]["state"], "done");
    assert_eq!(summary["tasks"][0]["notes"][0]["text"], "made progress");
}

#[test]
fn session_summary_human_format_lists_tasks_and_notes() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "note", "ship-the-release", "made progress"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "session", "summary"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("sprint"));
    assert!(stdout.contains("0/1 done"));
    assert!(stdout.contains("ship-the-release"));
    assert!(stdout.contains("made progress"));
}

#[test]
fn session_summary_by_explicit_id_works_with_no_open_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "finish", "sprint"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "session", "summary", "sprint", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["total"], 1);
}

#[test]
fn session_summary_orders_tasks_parent_before_children() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Parent", "--no-parent"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child", "--parent", "parent"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "session", "summary", "--format", "json"])
        .output()
        .unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["tasks"][0]["slug"], "parent");
    assert_eq!(summary["tasks"][1]["slug"], "child");
}

#[test]
fn session_summary_on_empty_session_has_no_tasks() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");

    let out = llmenv(dir.path())
        .args(["task", "session", "summary", "--format", "json"])
        .output()
        .unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["total"], 0);
    assert!(summary["tasks"].as_array().unwrap().is_empty());
}

#[test]
fn session_summary_unknown_id_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "summary", "no-such-session"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no session"));
}

#[test]
fn session_summary_with_no_open_session_and_no_id_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "summary"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no open session"));
}

#[test]
fn tasks_added_during_a_session_are_tagged_and_survive_it_finishing() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "In the session"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "finish", "sprint"])
        .assert()
        .success();

    // The task keeps its session tag as a historical record.
    let show = llmenv(dir.path())
        .args(["task", "show", "in-the-session"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert!(task["session"].is_string());

    // No session is open now, so a bare `task add` errors.
    llmenv(dir.path())
        .args(["task", "add", "After the session"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("session start"));
}

// --- task clear (#905) ---

#[test]
fn clear_by_id_deletes_the_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Throwaway task"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "clear", "throwaway-task"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Cleared task"));

    llmenv(dir.path())
        .args(["task", "show", "throwaway-task"])
        .assert()
        .failure();
}

#[test]
fn clear_by_session_deletes_only_that_sessions_tasks() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "doomed sprint");
    llmenv(dir.path())
        .args(["task", "add", "In the doomed sprint"])
        .assert()
        .success();
    // A second, concurrent session holds the survivor.
    llmenv(dir.path())
        .args(["task", "session", "start", "survivor sprint", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args([
            "task",
            "add",
            "Unrelated survivor",
            "--session",
            "survivor-sprint",
        ])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "clear", "--session", "doomed-sprint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Cleared 1 task(s)"));

    llmenv(dir.path())
        .args(["task", "show", "in-the-doomed-sprint"])
        .assert()
        .failure();
    llmenv(dir.path())
        .args(["task", "show", "unrelated-survivor"])
        .assert()
        .success();
}

#[test]
fn clear_with_neither_ids_nor_session_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "clear"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("specify one or more task ids"));
}

#[test]
fn clear_with_both_ids_and_session_is_rejected_by_clap() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "clear", "some-id", "--session", "some-session"])
        .assert()
        .failure();
}

// #1124: `ls` must not silently default to every session's tasks -- force a
// deliberate choice between --session <id> and --all.
#[test]
fn ls_with_neither_session_nor_all_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "ls"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--session"))
        .stderr(predicates::str::contains("--all"));
}

#[test]
fn ls_with_all_flag_succeeds() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "ls", "--all", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Ship the release"));
}

#[test]
fn ls_with_all_and_session_is_rejected_by_clap() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "ls", "--all", "--session", "some-session"])
        .assert()
        .failure();
}

// --current-project alone doesn't satisfy the requirement -- it narrows by
// project, not by session, so it's still "everything in this project" rather
// than the single deliberate choice #1124 wants to force.
#[test]
fn ls_with_only_current_project_still_requires_session_or_all() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "ls", "--current-project"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--session"));
}

#[test]
fn wait_marks_task_waiting_and_notes_reason() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "ship-the-release"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "wait", "ship-the-release", "waiting on spec review"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Marked"));

    let show = llmenv(dir.path())
        .args(["task", "show", "ship-the-release"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["state"], "waiting");
    assert!(
        task["notes"][0]["text"]
            .as_str()
            .unwrap()
            .contains("waiting on spec review")
    );
}

#[test]
fn add_guard_warns_for_wip_but_not_waiting_tasks() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "First task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "first-task"])
        .assert()
        .success();

    // A `wip` task should trip the "already in progress" guard — only for
    // an explicit --no-parent (#929): a bare `add` now chains onto "First
    // task" by default, which is no longer the guard's "unrelated" case.
    llmenv(dir.path())
        .args(["task", "add", "Second task", "--no-parent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress"));

    // Park it as `waiting` — the agent may legitimately start new work while
    // it's paused on something external, so the guard must stay silent (#933).
    llmenv(dir.path())
        .args(["task", "wait", "first-task", "blocked on review"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Third task", "--no-parent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress").not());
}

#[test]
fn wait_on_done_task_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "ship-the-release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "wait", "ship-the-release", "too late"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already done"));
}

#[test]
fn start_resumes_a_waiting_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "wait", "ship-the-release", "blocked"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "ship-the-release"])
        .assert()
        .success();

    let show = llmenv(dir.path())
        .args(["task", "show", "ship-the-release"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["state"], "wip");
}

// #1164: CLI-level coverage for --force and the parent soft-block warning
// (the underlying behavior is covered at the module level in src/task/mod.rs
// -- this exercises that the CLI flag actually reaches start_task, and that
// the warning text actually reaches the user).

#[test]
fn start_on_blocked_task_fails_without_force() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Blocker"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Blocked"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "block", "blocked", "--on", "blocker"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "start", "blocked"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("blocker"))
        .stderr(predicates::str::contains("--force"));
}

#[test]
fn start_on_blocked_task_with_force_succeeds() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Blocker"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Blocked"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "block", "blocked", "--on", "blocker"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "start", "blocked", "--force"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Started"));
}

#[test]
fn start_on_child_with_undone_parent_warns_but_starts() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Parent step"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child step", "--parent", "parent-step"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "start", "child-step"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Note: parent task 'parent-step' isn't done yet",
        ))
        .stdout(predicates::str::contains("Started"));
}

#[test]
fn start_on_child_with_done_parent_has_no_warning() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Parent step"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child step", "--parent", "parent-step"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "parent-step"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "parent-step"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "start", "child-step"])
        .assert()
        .success()
        .stdout(predicates::str::contains("isn't done yet").not());
}

#[test]
fn ls_filters_by_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint 1");
    llmenv(dir.path())
        .args(["task", "add", "In the session"])
        .assert()
        .success();
    // A second concurrent session holds the other task.
    llmenv(dir.path())
        .args(["task", "session", "start", "sprint 2", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args([
            "task",
            "add",
            "In the other session",
            "--session",
            "sprint-2",
        ])
        .assert()
        .success();

    let ls_json = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--session", "sprint-1"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls_json.stdout).unwrap();
    let tasks = tasks.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "In the session");
}

#[test]
fn ls_all_shows_tasks_across_sessions() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint 1");
    llmenv(dir.path())
        .args(["task", "add", "In the session"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "sprint 2", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "In the other", "--session", "sprint-2"])
        .assert()
        .success();

    let ls_json = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--all"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls_json.stdout).unwrap();
    assert_eq!(tasks.as_array().unwrap().len(), 2);
}

// --- task ls: human output, grouping, glyphs, filtering (#926) ---

/// Run `task ls` (+ extra args) with color forced off; return stdout as a String.
/// Adds `--all` unless `extra` already passes `--session` (they conflict).
fn ls(dir: &std::path::Path, extra: &[&str]) -> String {
    let mut args = vec!["task", "ls"];
    if !extra.contains(&"--session") {
        args.push("--all");
    }
    args.extend_from_slice(extra);
    let out = llmenv(dir)
        .env("NO_COLOR", "1")
        .args(&args)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn ls_human_groups_by_session_with_glyphs_labels_and_indented_subtasks() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Parent epic"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child step", "--parent", "parent-epic"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "parent-epic"])
        .assert()
        .success();

    let out = ls(dir.path(), &[]);
    // Session header present.
    assert!(out.contains("sprint"), "expected session header:\n{out}");
    // State labels rendered.
    assert!(out.contains("wip"), "expected wip label:\n{out}");
    assert!(out.contains("open"), "expected open label:\n{out}");
    // Subtask indented deeper than its parent.
    let parent_line = out.lines().find(|l| l.contains("parent-epic")).unwrap();
    let child_line = out.lines().find(|l| l.contains("child-step")).unwrap();
    let indent = |l: &str| l.len() - l.trim_start().len();
    assert!(
        indent(child_line) > indent(parent_line),
        "child not indented under parent:\n{out}"
    );
}

#[test]
fn ls_marks_blocked_tasks_with_their_refs() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Upstream"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Downstream"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "block", "downstream", "--on", "upstream"])
        .assert()
        .success();

    let out = ls(dir.path(), &[]);
    assert!(
        out.contains("blocked on: upstream"),
        "expected blocked annotation:\n{out}"
    );
}

#[test]
fn ls_hide_done_and_active_alias_hide_completed() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Keep me"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Finish me"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "finish-me"])
        .assert()
        .success();

    for flag in ["--hide-done", "--active"] {
        let out = ls(dir.path(), &[flag]);
        assert!(
            out.contains("keep-me"),
            "{flag} dropped active task:\n{out}"
        );
        assert!(
            !out.contains("finish-me"),
            "{flag} did not hide done task:\n{out}"
        );
    }
}

#[test]
fn ls_state_filter_is_repeatable() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "An open one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "A wip one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "a-wip-one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "A waiting one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "wait", "a-waiting-one", "blocked"])
        .assert()
        .success();

    let out = ls(dir.path(), &["--state", "wip", "--state", "waiting"]);
    assert!(out.contains("a-wip-one"), "{out}");
    assert!(out.contains("a-waiting-one"), "{out}");
    assert!(
        !out.contains("an-open-one"),
        "open task leaked past filter:\n{out}"
    );
}

#[test]
fn ls_state_filter_composes_with_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "alpha");
    llmenv(dir.path())
        .args(["task", "add", "Alpha wip"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "alpha-wip"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Alpha open"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "beta", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Beta wip", "--session", "beta"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "beta-wip"])
        .assert()
        .success();

    // Only alpha's wip task: session narrows to alpha, state filter to wip.
    let out = ls(dir.path(), &["--session", "alpha", "--state", "wip"]);
    assert!(out.contains("alpha-wip"), "{out}");
    assert!(
        !out.contains("alpha-open"),
        "state filter failed within session:\n{out}"
    );
    assert!(!out.contains("beta-wip"), "session filter failed:\n{out}");
}

#[test]
fn ls_empty_prints_no_tasks() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    assert_eq!(ls(dir.path(), &[]).trim(), "No tasks.");
    // A filter that matches nothing also yields the empty message.
    llmenv(dir.path())
        .args(["task", "add", "Only open"])
        .assert()
        .success();
    assert_eq!(ls(dir.path(), &["--state", "done"]).trim(), "No tasks.");
}

#[test]
fn ls_human_output_has_no_ansi_escapes_when_color_disabled() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Plain task"])
        .assert()
        .success();
    let out = ls(dir.path(), &[]);
    assert!(
        !out.contains('\u{1b}'),
        "unexpected ANSI escape in no-color output:\n{out:?}"
    );
}

#[test]
fn ls_json_applies_filters_only_when_passed() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Open one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Wip one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "wip-one"])
        .assert()
        .success();

    // No filter: both tasks in the stable machine format.
    let all = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--all"])
        .output()
        .unwrap();
    let all: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 2);

    // Filter passed: applies to JSON too.
    let filtered = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--state", "wip", "--all"])
        .output()
        .unwrap();
    let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    let arr = filtered.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["slug"], "wip-one");
}

#[test]
fn ls_respects_color_flag_over_tty_detection() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Colored task"])
        .assert()
        .success();

    // --color always forces ANSI even though stdout is piped (not a TTY).
    let always = llmenv(dir.path())
        .args(["--color", "always", "task", "ls", "--all"])
        .output()
        .unwrap();
    let always = String::from_utf8(always.stdout).unwrap();
    assert!(
        always.contains('\u{1b}'),
        "expected ANSI with --color always:\n{always:?}"
    );

    // --color never suppresses ANSI regardless of environment.
    let never = llmenv(dir.path())
        .args(["--color", "never", "task", "ls", "--all"])
        .output()
        .unwrap();
    let never = String::from_utf8(never.stdout).unwrap();
    assert!(
        !never.contains('\u{1b}'),
        "unexpected ANSI with --color never:\n{never:?}"
    );
}

// --- task ls --current-project (#1117, #927) ---

fn ls_json(dir: &std::path::Path, extra: &[&str]) -> Vec<serde_json::Value> {
    let mut args = vec!["task", "ls", "--format", "json"];
    if !extra.contains(&"--session") {
        args.push("--all");
    }
    args.extend_from_slice(extra);
    let out = llmenv(dir).args(&args).output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn ls_current_project_matches_a_task_in_the_current_projects_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "in this project");
    llmenv(dir.path())
        .args(["task", "add", "My task"])
        .assert()
        .success();

    let tasks = ls_json(dir.path(), &["--current-project"]);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "My task");
}

#[test]
fn ls_current_project_excludes_a_task_from_a_different_project() {
    let dir = TempDir::new().unwrap();
    let other_project = TempDir::new().unwrap();
    start_session(dir.path(), "in this project");
    llmenv(dir.path())
        .args(["task", "add", "My task"])
        .assert()
        .success();

    // A session (and task) started from a different cwd resolves to a
    // different project tag, even against the same LLMENV_STATE_DIR.
    llmenv(dir.path())
        .current_dir(other_project.path())
        .args(["task", "session", "start", "elsewhere"])
        .assert()
        .success();
    llmenv(dir.path())
        .current_dir(other_project.path())
        .args(["task", "add", "Someone else's task"])
        .assert()
        .success();

    let tasks = ls_json(dir.path(), &["--current-project"]);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "My task");
}

#[test]
fn ls_current_project_excludes_a_task_with_no_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Tagged"])
        .assert()
        .success();

    // Simulate a legacy task predating mandatory sessions (`session: null`).
    let legacy_path = dir.path().join("tasks").join("legacy-task.json");
    std::fs::write(
        &legacy_path,
        r#"{"slug":"legacy-task","title":"Legacy","state":"open",
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let tasks = ls_json(dir.path(), &["--current-project"]);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "Tagged");
}

#[test]
fn ls_current_project_includes_a_task_from_a_finished_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "old sprint");
    llmenv(dir.path())
        .args(["task", "add", "Old task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "finish", "old-sprint"])
        .assert()
        .success();

    // No open session for this project at all — --current-project must
    // still surface a finished session's task, not require an open one.
    let tasks = ls_json(dir.path(), &["--current-project"]);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "Old task");
}

#[test]
fn ls_current_project_with_no_sessions_at_all_is_empty_not_an_error() {
    let dir = TempDir::new().unwrap();
    let tasks = ls_json(dir.path(), &["--current-project"]);
    assert_eq!(tasks.len(), 0);
}

#[test]
fn ls_current_project_composes_with_session_and_state_filters() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint 1");
    llmenv(dir.path())
        .args(["task", "add", "Task one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task two"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "sprint 2", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task three", "--session", "sprint-2"])
        .assert()
        .success();

    let tasks = ls_json(
        dir.path(),
        &[
            "--current-project",
            "--session",
            "sprint-1",
            "--state",
            "wip",
        ],
    );
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "Task one");
}

// --- task show --current / --next (#1117, #928) ---

#[test]
fn show_current_resolves_the_wip_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task two"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-one"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--current"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["slug"], "task-one");
}

#[test]
fn show_current_falls_back_to_most_recently_updated_non_done_task_without_a_wip_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task two"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "note", "task-two", "a note"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--current"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["slug"], "task-two");
}

#[test]
fn show_current_with_no_open_session_fails() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "show", "--current"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no open session"));
}

#[test]
fn show_current_and_next_are_mutually_exclusive() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "show", "--current", "--next"])
        .assert()
        .failure();
}

#[test]
fn show_current_conflicts_with_a_positional_id() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "show", "some-id", "--current"])
        .assert()
        .failure();
}

#[test]
fn show_neither_id_nor_current_nor_next_errors() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "show"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--current"));
}

#[test]
fn show_next_skips_blocked_and_done_tasks() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    // Slugs are numbered so their alphabetical order matches creation order
    // even if two tasks land in the same second-precision `created_at`.
    llmenv(dir.path())
        .args(["task", "add", "Task 1 current"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-1-current"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task 2 blocked"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "block", "task-2-blocked", "--on", "task-1-current"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task 3 finished"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "task-3-finished"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task 4 actionable"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--next"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["slug"], "task-4-actionable");
}

#[test]
fn show_next_prefers_child_over_next_sibling() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Parent epic"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "parent-epic"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Child step", "--parent", "parent-epic"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Sibling task"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--next"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["slug"], "child-step");
}

#[test]
fn show_next_skips_a_waiting_task() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Task 1 current"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-1-current"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task 2 waiting"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-2-waiting"])
        .assert()
        .success();
    llmenv(dir.path())
        .args([
            "task",
            "wait",
            "task-2-waiting",
            "blocked on a human review",
        ])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task 3 actionable"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--next"])
        .output()
        .unwrap();
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        task["slug"], "task-3-actionable",
        "a waiting task must not be handed back as the next actionable step"
    );
}

#[test]
fn show_current_sanitizes_a_control_character_in_the_session_name() {
    let dir = TempDir::new().unwrap();
    // A session name containing a control character (here, a raw escape)
    // must not reach the terminal unsanitized in the multi-session header.
    llmenv(dir.path())
        .args(["task", "session", "start", "evil\u{1b}[31mred"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-one"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "second session", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Task two", "--session", "second-session"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "task-two"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--current"])
        .output()
        .unwrap();
    assert!(
        !out.stdout.contains(&0x1b),
        "raw control byte leaked into the session header:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn show_current_with_no_qualifying_task_reports_none_without_erroring() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Only task"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "done", "only-task"])
        .assert()
        .success();

    llmenv(dir.path())
        .args(["task", "show", "--current"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No current task."));
}

#[test]
fn show_current_shows_a_separate_block_per_open_session_for_the_project() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint 1");
    llmenv(dir.path())
        .args(["task", "add", "First"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "sprint 2", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Second", "--session", "sprint-2"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "start", "second"])
        .assert()
        .success();

    let out = llmenv(dir.path())
        .args(["task", "show", "--current"])
        .output()
        .unwrap();
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(out.contains("sprint 1"), "expected sprint 1 header:\n{out}");
    assert!(out.contains("sprint 2"), "expected sprint 2 header:\n{out}");
    assert!(
        out.contains("\"slug\": \"first\""),
        "expected first task:\n{out}"
    );
    assert!(
        out.contains("\"slug\": \"second\""),
        "expected second task:\n{out}"
    );
    assert!(
        out.contains("---"),
        "expected a separator between session blocks:\n{out}"
    );
}
