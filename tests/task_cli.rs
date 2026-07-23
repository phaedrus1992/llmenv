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

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use tempfile::TempDir;

fn llmenv(state_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_STATE_DIR", state_dir);
    cmd
}

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

    llmenv(dir.path())
        .args(["task", "add", "Unrelated new thing"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already in progress"));
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
        .args(["task", "ls", "--format", "json"])
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

    // A `wip` task should trip the "already in progress" guard.
    llmenv(dir.path())
        .args(["task", "add", "Second task"])
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
        .args(["task", "add", "Third task"])
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
fn ls_unfiltered_shows_tasks_across_sessions() {
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
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls_json.stdout).unwrap();
    assert_eq!(tasks.as_array().unwrap().len(), 2);
}

// --- task ls: human output, grouping, glyphs, filtering (#926) ---

/// Run `task ls` (+ extra args) with color forced off; return stdout as a String.
fn ls(dir: &std::path::Path, extra: &[&str]) -> String {
    let mut args = vec!["task", "ls"];
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
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    let all: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 2);

    // Filter passed: applies to JSON too.
    let filtered = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--state", "wip"])
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
        .args(["--color", "always", "task", "ls"])
        .output()
        .unwrap();
    let always = String::from_utf8(always.stdout).unwrap();
    assert!(
        always.contains('\u{1b}'),
        "expected ANSI with --color always:\n{always:?}"
    );

    // --color never suppresses ANSI regardless of environment.
    let never = llmenv(dir.path())
        .args(["--color", "never", "task", "ls"])
        .output()
        .unwrap();
    let never = String::from_utf8(never.stdout).unwrap();
    assert!(
        !never.contains('\u{1b}'),
        "unexpected ANSI with --color never:\n{never:?}"
    );
}
