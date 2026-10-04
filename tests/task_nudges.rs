#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! Drives `llmenv hook-run` through sequences of tool calls and checks the task tracker nudges
//! and the commit deny-once (#2456).

use std::fs;

use predicates::prelude::PredicateBooleanExt;
use tempfile::TempDir;

mod support;
use support::isolated_llmenv_cmd as llmenv;

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

/// A config with the task tracker on and the given extra `task_tracker` lines.
fn setup(tracker_extra: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let config = format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

features:
  task_tracker:
    enabled: true
{tracker_extra}
"#,
        user = user()
    );
    fs::write(dir.path().join("config.yaml"), config).unwrap();
    dir
}

fn hook(dir: &TempDir, event: &str, payload: &serde_json::Value) -> assert_cmd::assert::Assert {
    llmenv(dir.path())
        .env("LLMENV_CONFIG", dir.path().join("config.yaml"))
        .args(["hook-run", event])
        .write_stdin(payload.to_string())
        .timeout(std::time::Duration::from_secs(20))
        .assert()
        .success()
}

fn post(tool: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-1",
        "tool_name": tool,
        "tool_input": input,
    })
}

fn pre_bash(command: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-1",
        "tool_name": "Bash",
        "tool_input": { "command": command },
    })
}

fn task(dir: &TempDir, args: &[&str]) {
    llmenv(dir.path()).arg("task").args(args).assert().success();
}

// Behavior 1: open a session as soon as the work has more than one part.
#[test]
fn a_workflow_skill_with_no_session_gets_the_exact_commands() {
    let dir = setup("");
    hook(
        &dir,
        "post_tool_use",
        &post("Skill", serde_json::json!({ "skill": "dev-sprint" })),
    )
    .stdout(predicates::str::contains("llmenv task session start"))
    .stdout(predicates::str::contains("--task"));
    // Once for each session.
    hook(
        &dir,
        "post_tool_use",
        &post("Skill", serde_json::json!({ "skill": "ship-issue" })),
    )
    .stdout(predicates::str::contains("llmenv task").not());
}

#[test]
fn untracked_work_gets_a_nudge_after_n_mutating_calls() {
    let dir = setup("    nudge_after: 2\n");
    let edit = post("Edit", serde_json::json!({}));
    hook(&dir, "post_tool_use", &edit).stdout(predicates::str::contains("llmenv task").not());
    hook(&dir, "post_tool_use", &edit).stdout(predicates::str::contains(
        "2 tool calls changed the project",
    ));
}

#[test]
fn the_nudge_switch_turns_the_nudges_off() {
    let dir = setup("    nudge_after: 1\n    nudges: false\n");
    hook(&dir, "post_tool_use", &post("Edit", serde_json::json!({})))
        .stdout(predicates::str::contains("llmenv task").not());
}

// Behavior 2: a session with zero tasks is an error state.
#[test]
fn an_open_session_with_no_tasks_is_named_at_stop() {
    let dir = setup("");
    task(&dir, &["session", "start", "sprint"]);
    hook(
        &dir,
        "stop",
        &serde_json::json!({ "hook_event_name": "Stop", "session_id": "sess-1" }),
    )
    .stdout(predicates::str::contains("is open and has no tasks"))
    .stdout(predicates::str::contains("llmenv task add"));
}

// Behavior 3: do not move on while the previous task is open. The first commit with no task in
// progress is denied once.
#[test]
fn the_first_commit_with_no_task_in_progress_is_denied_once() {
    let dir = setup("");
    hook(
        &dir,
        "pre_tool_use",
        &pre_bash("git add -A && git commit -m x"),
    )
    .stdout(predicates::str::contains("\"permissionDecision\":\"deny\""))
    .stdout(predicates::str::contains("llmenv task session start"));
    hook(
        &dir,
        "pre_tool_use",
        &pre_bash("git add -A && git commit -m x"),
    )
    .stdout(predicates::str::contains("deny").not());
}

#[test]
fn a_task_in_progress_lets_the_commit_through_and_the_switch_disables_the_deny() {
    let dir = setup("");
    task(&dir, &["session", "start", "sprint", "--task", "Ship it"]);
    task(&dir, &["start", "ship-it"]);
    hook(&dir, "pre_tool_use", &pre_bash("git commit -m x"))
        .stdout(predicates::str::contains("deny").not());

    let off = setup("    enforce_commit: false\n");
    hook(&off, "pre_tool_use", &pre_bash("gh pr create --title x"))
        .stdout(predicates::str::contains("deny").not());
}

// Behavior 4: tell the task it is waiting when the user must answer.
#[test]
fn a_question_to_the_user_asks_the_agent_to_park_the_task() {
    let dir = setup("");
    task(&dir, &["session", "start", "sprint", "--task", "Ship it"]);
    task(&dir, &["start", "ship-it"]);
    hook(
        &dir,
        "post_tool_use",
        &post("AskUserQuestion", serde_json::json!({})),
    )
    .stdout(predicates::str::contains("llmenv task wait ship-it"))
    .stdout(predicates::str::contains("llmenv task start ship-it"));
    hook(
        &dir,
        "stop",
        &serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "sess-1",
            "last_assistant_message": "Which option do you prefer?",
        }),
    )
    .stdout(predicates::str::contains("llmenv task wait ship-it"));
}
