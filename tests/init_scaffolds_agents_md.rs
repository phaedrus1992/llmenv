#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use tempfile::TempDir;

#[test]
fn test_init_scaffolds_agents_md_file() {
    let temp = TempDir::new().expect("create temp dir");
    let config_dir = temp.path().to_str().expect("path to string");

    // Run the init command via CLI
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmenv"))
        .arg("init")
        .arg(config_dir)
        .output()
        .expect("run init command");

    assert!(
        output.status.success(),
        "init should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check that both config.yaml and AGENTS.md were created
    let config_path = std::path::PathBuf::from(config_dir).join("config.yaml");
    assert!(config_path.exists(), "config.yaml should exist");

    let agents_path = std::path::PathBuf::from(config_dir).join("AGENTS.md");
    assert!(
        agents_path.exists(),
        "AGENTS.md should be created alongside config.yaml"
    );

    // Verify AGENTS.md has meaningful content
    let content = fs::read_to_string(&agents_path).expect("read AGENTS.md");
    assert!(
        content.contains("AGENTS") || content.contains("Agent"),
        "AGENTS.md should contain agent-related content"
    );
}

#[test]
fn test_init_skips_existing_agents_md() {
    let temp = TempDir::new().expect("create temp dir");
    let config_dir = temp.path();

    // Pre-create AGENTS.md with custom content
    let agents_path = config_dir.join("AGENTS.md");
    let original_content = "# My Custom Agent Guide\n";
    fs::write(&agents_path, original_content).expect("write custom agents file");

    // Also create config.yaml to prevent init skipping
    let config_path = config_dir.join("config.yaml");
    fs::write(&config_path, "cache: { cache_dir: ~/.cache/llmenv }").expect("write config");

    // Run init — should skip existing AGENTS.md
    let config_dir_str = config_dir.to_str().expect("path to string");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmenv"))
        .arg("init")
        .arg(config_dir_str)
        .output()
        .expect("run init command");

    // init should succeed (it skips if config exists)
    // The custom AGENTS.md should remain unchanged
    let content = fs::read_to_string(&agents_path).expect("read agents file");
    assert_eq!(
        content, original_content,
        "existing AGENTS.md should not be overwritten"
    );
}
