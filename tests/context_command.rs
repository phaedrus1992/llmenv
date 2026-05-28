use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Create a test config with given content and return (dir, config_path)
fn setup_config(content: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, content).unwrap();
    (dir, config_path)
}

#[test]
fn context_shows_active_scopes() {
    let config = r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: ranger
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
"#;

    let (_dir, config_path) = setup_config(config);
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
    let config = r#"
scope:
  network: []
  host: []
  user:
    - id: active-user
      match:
        user: ranger
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
"#;

    let (_dir, config_path) = setup_config(config);
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
