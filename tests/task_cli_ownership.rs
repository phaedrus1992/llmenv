#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! Task ownership across conversations (#3480, #3481, #3486): the `--other-session` override,
//! the session picker for `task add`, and parent links across sessions.

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

// --- #3481: clear --other-session leaves a record on the owning session ---

#[test]
fn clear_with_other_session_notes_the_owning_session() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "add", "doomed"])
        .assert()
        .success();

    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args(["task", "clear", "doomed", "--other-session", "first"])
        .assert()
        .success();

    let shown = llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "show", "first"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        text.contains("Cleared with --other-session") && text.contains("doomed"),
        "the owning session must name the caller and the deleted task:\n{text}"
    );
}

#[test]
fn clear_note_lists_only_the_tasks_that_needed_the_override() {
    let dir = TempDir::new().unwrap();
    let run = |conv: &str, args: &[&str]| {
        llmenv(dir.path())
            .env("CLAUDE_CODE_SESSION_ID", conv)
            .args(args)
            .assert()
            .success();
    };
    run("conv-old", &["task", "session", "start", "first"]);
    run("conv-old", &["task", "add", "theirs"]);
    run("conv-new", &["task", "session", "start", "second", "--new"]);
    run("conv-new", &["task", "add", "mine"]);

    run(
        "conv-new",
        &[
            "task",
            "clear",
            "mine",
            "theirs",
            "--other-session",
            "first",
        ],
    );

    let shown = llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "show", "first"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        text.contains("Deleted tasks: theirs."),
        "the note must list only the task that needed the override:\n{text}"
    );
}

// --- #3480: the picker and the parent link respect ownership ---

#[test]
fn add_without_session_refuses_the_sole_session_of_another_conversation() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "start", "first"])
        .assert()
        .success();

    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args(["task", "add", "y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("belongs to another conversation"));
}

#[test]
fn add_with_a_parent_in_another_session_is_refused() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "add", "parent-task"])
        .assert()
        .success();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args(["task", "session", "start", "second", "--new"])
        .assert()
        .success();

    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args(["task", "add", "child", "--parent", "parent-task"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot be the parent of a task in another session",
        ));
}

// --- #3486: task add --session names a session of another conversation ---

#[test]
fn add_to_a_session_of_another_conversation_is_refused_without_other_session() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "start", "first"])
        .assert()
        .success();

    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args(["task", "add", "x", "--session", "first"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("belongs to another conversation"))
        .stderr(predicates::str::contains("--other-session <SESSION_ID>"));
}

#[test]
fn add_to_a_session_of_another_conversation_succeeds_with_other_session() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-old")
        .args(["task", "session", "start", "first"])
        .assert()
        .success();

    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args([
            "task",
            "add",
            "x",
            "--session",
            "first",
            "--other-session",
            "first",
        ])
        .assert()
        .success();
    let ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json", "--all"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert_eq!(tasks[0]["session"], "first", "{tasks}");
    let shown = llmenv(dir.path())
        .args(["task", "show", "x"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        text.contains("--other-session"),
        "the override must leave a note on the new task:\n{text}"
    );
}

#[test]
fn add_to_an_unowned_session_needs_no_other_session() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "shared");

    llmenv(dir.path())
        .env("CLAUDE_CODE_SESSION_ID", "conv-new")
        .args(["task", "add", "x", "--session", "shared"])
        .assert()
        .success();
}
