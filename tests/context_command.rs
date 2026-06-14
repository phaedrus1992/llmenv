#![expect(clippy::unwrap_used, reason = "test scaffolding")]
use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Get current user for test config
fn get_current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

/// Create a test config with given content and return (dir, config_path)
fn setup_config(content: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, content).unwrap();
    (dir, config_path)
}

#[test]
fn context_shows_active_scopes() {
    let current_user = get_current_user();
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
  project: []

tag:
  test: ""

bundle:
  - name: test-bundle
    tags: [test]

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user
    );

    let (_dir, config_path) = setup_config(&config);
    let config_dir = _dir.path();

    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .arg("context");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Active"));
}

#[test]
fn context_shows_inactive_scopes() {
    let current_user = get_current_user();
    let config = format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: active-user
      match:
        user: {user}
      tags: [active]
    - id: inactive-user
      match:
        user: nonexistent123456
      tags: [inactive]
  project: []

tag:
  active: ""
  inactive: ""

bundle:
  - name: active-bundle
    tags: [active]
  - name: inactive-bundle
    tags: [inactive]

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user
    );

    let (_dir, config_path) = setup_config(&config);
    let config_dir = _dir.path();

    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .arg("context");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Active"))
        .stdout(predicate::str::contains("Inactive"));
}

#[test]
fn context_shows_merged_manifest_with_hooks() {
    let current_user = get_current_user();
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
  project: []

tag:
  test: ""

bundle:
  - name: test-bundle
    tags: [test]

# Top-level hooks that will appear in merged manifest
capabilities:
  hooks:
    - event: PostToolUse
      matcher: bash
      handler:
        type: command
        command: "echo test"

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user
    );

    let (_dir, config_path) = setup_config(&config);
    let config_dir = _dir.path();

    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .arg("context");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Merged Manifest"))
        .stdout(predicate::str::contains("Hooks"))
        .stdout(predicate::str::contains(
            "PostToolUse bash (from config.yaml)",
        ));
}
