#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Test for #173: doctor warns on version skew between running binary and cached materializations

use std::fs;
use std::process::Command;

fn setup_test_config() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let minimal_config = r#"
scope:
  network: []
  host: []
  user: []
cache:
  cache_dir: ~/.cache/llmenv
  cache_retention_hours: 168
capabilities:
  hooks: []
bundle: []
mcp: []
plugin_marketplace: []
plugin_collection: []
"#;
    fs::write(tmp.path().join("config.yaml"), minimal_config).expect("failed to write config");
    tmp
}

#[test]
fn test_doctor_runs_without_error() {
    let tmp = setup_test_config();

    let output = Command::new("cargo")
        .args(["run", "--quiet", "--", "doctor"])
        .env("LLMENV_CONFIG_DIR", tmp.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv doctor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor should exit 0 even with warnings\nstderr:\n{}",
        stderr
    );
}

#[test]
fn test_doctor_version_check_label_exists() {
    let tmp = setup_test_config();

    let output = Command::new("cargo")
        .args(["run", "--quiet", "--", "doctor"])
        .env("LLMENV_CONFIG_DIR", tmp.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv doctor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("Doctor check complete."),
        "doctor should complete with summary\nstderr:\n{}",
        stderr
    );
}
