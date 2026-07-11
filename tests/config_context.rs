#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Integration tests for `llmenv config-context` (#419).
//!
//! Verifies that the hook JSON output places `hookEventName` inside
//! `hookSpecificOutput` (not at the top level), which is the structure
//! Claude Code requires for SessionStart hook payloads.

use assert_cmd::Command;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

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

    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_STATE_DIR", config_dir)
        .arg("config-context")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#);

    let output = cmd.timeout(Duration::from_secs(10)).output().unwrap();
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

#[test]
fn config_context_exits_zero_on_empty_stdin() {
    let (_dir, config_path) = setup_config();
    let config_dir = _dir.path();

    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_STATE_DIR", config_dir)
        .arg("config-context")
        .write_stdin("");

    let output = cmd.timeout(Duration::from_secs(10)).output().unwrap();
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
