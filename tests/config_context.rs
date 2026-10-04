#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Integration tests for `llmenv config-context` (#419).
//!
//! Verifies that the hook JSON output places `hookEventName` inside
//! `hookSpecificOutput` (not at the top level), which is the structure
//! Claude Code requires for SessionStart hook payloads.

use std::fs;
use tempfile::TempDir;

mod support;

fn setup_config() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(
        &config_path,
        "adapter:\n  engine: claude-code\nscope:\n  network: []\n  host: []\n  user: []\n",
    )
    .unwrap();
    (dir, config_path)
}

#[test]
fn config_context_places_hook_event_name_inside_hook_specific_output() {
    let (_dir, config_path) = setup_config();
    let config_dir = _dir.path();

    let mut cmd = support::isolated_llmenv_cmd(config_dir);
    cmd.env("LLMENV_CONFIG", &config_path)
        .arg("config-context")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#);

    let output = cmd.output().unwrap();
    assert!(output.status.success(), "config-context must exit 0");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("config-context output must be valid JSON: {e}\ngot: {stdout}"));

    assert!(
        parsed.get("hookEventName").is_none(),
        "hookEventName must not appear at top level; got: {parsed}"
    );
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"].as_str(),
        Some("SessionStart"),
        "hookEventName must be inside hookSpecificOutput"
    );
    assert!(
        parsed["hookSpecificOutput"]
            .get("additionalContext")
            .is_some(),
        "hookSpecificOutput must contain additionalContext"
    );
}

// #231: the task-tracker SessionStart reminder rides the config-context hook. (hook-run's own
// SessionStart output is accepted too since #2251, but this channel predates it.)
#[test]
fn config_context_includes_task_tracker_reminder_for_wip_tasks() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(
        &config_path,
        "adapter:\n  engine: claude-code\nscope:\n  network: []\n  host: []\n  user: []\n\
         features:\n  task_tracker:\n    enabled: true\n",
    )
    .unwrap();
    let state_dir = TempDir::new().unwrap();

    support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .args(["task", "session", "start", "sprint"])
        .assert()
        .success();
    support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .args(["task", "add", "Left over from last session"])
        .assert()
        .success();
    support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .args(["task", "start", "left-over-from-last"])
        .assert()
        .success();

    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .arg("config-context")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let ctx = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        ctx.contains("Left over from last session"),
        "additionalContext must mention the wip task; got: {ctx}"
    );
}

#[test]
fn config_context_no_task_tracker_reminder_when_disabled() {
    let (dir, config_path) = setup_config();
    let state_dir = TempDir::new().unwrap();

    support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .args(["task", "session", "start", "sprint"])
        .assert()
        .success();
    support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .args(["task", "add", "Some open task"])
        .assert()
        .success();

    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .arg("config-context")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let ctx = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        !ctx.contains("Some open task"),
        "additionalContext must not mention tasks when task_tracker disabled; got: {ctx}"
    );
}

#[test]
fn config_context_exits_zero_on_empty_stdin() {
    let (_dir, config_path) = setup_config();
    let config_dir = _dir.path();

    let mut cmd = support::isolated_llmenv_cmd(config_dir);
    cmd.env("LLMENV_CONFIG", &config_path)
        .arg("config-context")
        .write_stdin("");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "config-context must exit 0 on empty stdin"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("must be valid JSON: {e}\ngot: {stdout}"));

    assert!(
        parsed.get("hookEventName").is_none(),
        "hookEventName must not appear at top level on empty stdin; got: {parsed}"
    );
    assert!(
        parsed["hookSpecificOutput"]["hookEventName"]
            .as_str()
            .is_some(),
        "hookEventName must be present inside hookSpecificOutput"
    );
}

// #2339: the SessionStart reminder carries each open session's resume context and the command
// that follows each ref, so a fresh agent after `/clear` does not rebuild it by hand.
#[test]
fn config_context_includes_resume_context_for_an_open_session() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(
        &config_path,
        "adapter:\n  engine: claude-code\nscope:\n  network: []\n  host: []\n  user: []\n\
         features:\n  task_tracker:\n    enabled: true\n",
    )
    .unwrap();
    let state_dir = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .current_dir(cwd.path())
        .args([
            "task",
            "session",
            "start",
            "sprint",
            "--context",
            "pick up at step 4",
            "--issue",
            "2339",
        ])
        .assert()
        .success();

    let output = support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .current_dir(cwd.path())
        .arg("config-context")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .output()
        .unwrap();
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    let ctx = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(ctx.contains("pick up at step 4"), "got: {ctx}");
    assert!(ctx.contains("gh issue view 2339"), "got: {ctx}");
}

fn context_for(tracker_yaml: &str) -> String {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(
        &config_path,
        format!(
            "adapter:\n  engine: claude-code\nscope:\n  network: []\n  host: []\n  user: []\n{tracker_yaml}"
        ),
    )
    .unwrap();
    let state_dir = TempDir::new().unwrap();
    let output = support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_STATE_DIR", state_dir.path())
        .arg("config-context")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .output()
        .unwrap();
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

// #2457: with the tracker on and an empty personal config, SessionStart carries the four behaviors
// and their commands. The text comes from llmenv, not from a bundle.
#[test]
fn config_context_carries_the_core_task_rules_with_an_empty_config() {
    let ctx = context_for("features:\n  task_tracker:\n    enabled: true\n");
    for needle in [
        "llmenv task session start",
        "llmenv task add",
        "llmenv task start <slug>",
        "llmenv task done <slug>",
        "llmenv task wait <slug>",
        "redirected to `llmenv task`",
    ] {
        assert!(ctx.contains(needle), "missing {needle:?}: {ctx}");
    }
}

#[test]
fn config_context_core_rules_follow_the_nudges_switch_and_the_redirect_switch() {
    let off = context_for("features:\n  task_tracker:\n    enabled: true\n    nudges: false\n");
    assert!(!off.contains("llmenv task session start"), "{off}");
    let no_redirect = context_for(
        "features:\n  task_tracker:\n    enabled: true\n    block_engine_task_tools: false\n",
    );
    assert!(
        no_redirect.contains("llmenv task session start"),
        "{no_redirect}"
    );
    assert!(!no_redirect.contains("redirected to"), "{no_redirect}");
    let disabled = context_for("");
    assert!(
        !disabled.contains("llmenv task session start"),
        "{disabled}"
    );
}
