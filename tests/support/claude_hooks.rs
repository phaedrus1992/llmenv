//! Hook-table helpers for the Claude Code adapter integration tests.

use llmenv::adapter::AgentAdapter;
use llmenv::adapter::claude_code::ClaudeCodeAdapter;
use tempfile::tempdir;

// #985: the task-tool redirect PreToolUse hook is registered only when the
// task tracker is enabled. #980: also gated by the `block_engine_task_tools`
// opt-out sub-toggle.
pub fn task_tracker_matcher_present(enabled: bool, block_engine_task_tools: bool) -> bool {
    let m = llmenv::merge::MergedManifest {
        capabilities: llmenv::config::Capabilities {
            features: Some(llmenv::config::Features {
                task_tracker: Some(llmenv::config::TaskTracker {
                    enabled,
                    block_engine_task_tools,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");
    let parsed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json"),
    )
    .expect("parse settings.json");
    parsed["hooks"]["PreToolUse"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|e| e["matcher"].as_str() == Some("^(TaskCreate|TaskList|TaskUpdate)$"))
}

#[test]
pub fn task_tool_redirect_hook_registered_when_tracker_enabled() {
    assert!(
        task_tracker_matcher_present(true, true),
        "task-tool redirect hook must be registered when the tracker is enabled"
    );
}

#[test]
pub fn task_tool_redirect_hook_absent_when_tracker_disabled() {
    assert!(
        !task_tracker_matcher_present(false, true),
        "task-tool redirect hook must not be registered when the tracker is disabled"
    );
}

#[test]
pub fn task_tool_redirect_hook_absent_when_opted_out() {
    assert!(
        !task_tracker_matcher_present(true, false),
        "task-tool redirect hook must not be registered when \
         block_engine_task_tools is false, even with the tracker enabled"
    );
}

// #2456: the nudge and commit-deny hooks follow the tracker switches.
pub fn tracker_hooks(
    tracker: llmenv::config::TaskTracker,
    session_log_on: bool,
) -> serde_json::Value {
    tracker_hooks_with(tracker, session_log_on, None)
}

pub fn tracker_hooks_with(
    tracker: llmenv::config::TaskTracker,
    session_log_on: bool,
    slippage: Option<llmenv::config::SlippageControl>,
) -> serde_json::Value {
    let session_log = if session_log_on {
        llmenv::config::SessionLog::default()
    } else {
        llmenv::config::SessionLog {
            file: None,
            transcript: None,
            max_content_bytes: None,
        }
    };
    let m = llmenv::merge::MergedManifest {
        session_log,
        capabilities: llmenv::config::Capabilities {
            features: Some(llmenv::config::Features {
                task_tracker: Some(tracker),
                slippage,
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");
    let parsed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json"),
    )
    .expect("parse settings.json");
    parsed["hooks"].clone()
}

pub fn has_matcher(hooks: &serde_json::Value, event: &str, matcher: &str) -> bool {
    hooks[event]
        .as_array()
        .into_iter()
        .flatten()
        .any(|e| e["matcher"].as_str() == Some(matcher))
}

pub fn command_count(hooks: &serde_json::Value, event: &str, command: &str) -> usize {
    hooks[event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["hooks"].as_array().into_iter().flatten())
        .filter(|h| h["command"].as_str().is_some_and(|c| c.ends_with(command)))
        .count()
}

/// Replace the absolute llmenv program that a rendered hook command names with `llmenv`.
///
/// A hook command names the running llmenv binary (#3081), whose path differs per machine and
/// per build. A test process runs its own test binary, so that path is what gets redacted.
pub fn redact_llmenv_program(text: &str) -> String {
    match std::env::current_exe() {
        Ok(path) => text.replace(&path.display().to_string(), "llmenv"),
        Err(_) => text.to_string(),
    }
}
