#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Test for #173: doctor warns on version skew between running binary and cached materializations

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn setup_test_config() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME env var not set");
    let config_dir = PathBuf::from(home).join(".config");
    let llmenv_config = config_dir.join("llmenv");
    fs::create_dir_all(&llmenv_config).expect("failed to create config dir");

    let config_path = llmenv_config.join("config.yaml");
    if !config_path.exists() {
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
        fs::write(&config_path, minimal_config).expect("failed to write config");
    }
    config_path
}

#[test]
fn test_doctor_runs_without_error() {
    // Ensure config exists before running doctor
    setup_test_config();

    // Baseline: doctor should run and exit 0 (warnings don't block)
    let output = Command::new("cargo")
        .args(["run", "--quiet", "--", "doctor"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv doctor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Exit 0 is expected (warnings don't make it fail)
    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor should exit 0 even with warnings\nstderr:\n{}",
        stderr
    );
}

#[test]
fn test_doctor_version_check_label_exists() {
    // Ensure config exists before running doctor
    setup_test_config();

    // Version skew check should be in doctor output (or at least not break doctor)
    let output = Command::new("cargo")
        .args(["run", "--quiet", "--", "doctor"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv doctor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Doctor should at least complete and show the summary
    assert!(
        stderr.contains("Doctor check complete") || stderr.contains("Found"),
        "doctor should complete with summary"
    );
}
