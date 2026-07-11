#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Build a `Command` for `llmenv` with `LLMENV_CONFIG_DIR` and `LLMENV_STATE_DIR`
/// set to `config_dir`.
fn llmenv_cmd(config_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_STATE_DIR", config_dir);
    cmd
}

/// Assert the standard set of files created by `llmenv setup --no-launch`.
///
/// Mirrors the assertions in `src/cli/setup.rs::test_setup_no_launch_creates_files`.
fn assert_setup_files(config_dir: &Path) {
    assert!(
        config_dir.join("config.yaml").is_file(),
        "config.yaml should exist"
    );
    assert!(
        config_dir.join("AGENTS.md").is_file(),
        "AGENTS.md should exist"
    );
    assert!(
        config_dir.join(".llmenv-setup-state.json").is_file(),
        ".llmenv-setup-state.json should exist"
    );
    assert!(
        config_dir
            .join("bundles/base/skills/setup-llmenv/SKILL.md")
            .is_file(),
        "SKILL.md should exist"
    );
}

/// Basic `llmenv setup --no-launch` in an empty temp dir.
#[test]
fn setup_no_launch_creates_config() {
    let dir = TempDir::new().unwrap();
    llmenv_cmd(dir.path())
        .arg("setup")
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .success();
    assert_setup_files(dir.path());
}

/// `[PATH]` positional arg overrides the config dir without needing `LLMENV_CONFIG_DIR`.
#[test]
fn setup_custom_path() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("llmenv")
        .unwrap()
        .arg("setup")
        .arg(dir.path().to_str().unwrap())
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .success();
    assert_setup_files(dir.path());
}

/// `--repo` sets the marketplace without network access.
#[test]
fn setup_repo_flag_non_interactive() {
    let dir = TempDir::new().unwrap();
    let repo_url = "https://example.com/user/llmenv-config.git";
    llmenv_cmd(dir.path())
        .arg("setup")
        .arg("--repo")
        .arg(repo_url)
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .success();
    assert_setup_files(dir.path());
    let config = fs::read_to_string(dir.path().join("config.yaml"))
        .expect("config.yaml should exist after setup");
    assert!(
        config.contains(repo_url),
        "repo URL should appear in generated config"
    );
}

/// Rescan on an existing setup exits 0 and does not overwrite files.
#[test]
fn setup_rescan_on_existing() {
    let dir = TempDir::new().unwrap();

    // First run a full setup
    llmenv_cmd(dir.path())
        .arg("setup")
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .success();

    let config_original =
        fs::read_to_string(dir.path().join("config.yaml")).expect("config.yaml should exist");
    let agents_original =
        fs::read_to_string(dir.path().join("AGENTS.md")).expect("AGENTS.md should exist");

    // Now run rescan
    llmenv_cmd(dir.path())
        .arg("setup")
        .arg("--rescan")
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .success();

    // Files should be unchanged
    let config_after =
        fs::read_to_string(dir.path().join("config.yaml")).expect("config.yaml should still exist");
    let agents_after =
        fs::read_to_string(dir.path().join("AGENTS.md")).expect("AGENTS.md should still exist");
    assert_eq!(
        config_after, config_original,
        "config.yaml should not be modified by rescan"
    );
    assert_eq!(
        agents_after, agents_original,
        "AGENTS.md should not be modified by rescan"
    );
}

/// Rescan on an empty dir fails with a clear error message.
#[test]
fn setup_rescan_on_empty_dir() {
    let dir = TempDir::new().unwrap();
    llmenv_cmd(dir.path())
        .arg("setup")
        .arg("--rescan")
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Run `llmenv setup` first"));
}

/// A missing `LLMENV_CONFIG_DIR` must fail with an error and must not panic.
#[test]
fn setup_missing_config_dir() {
    let missing = Path::new("/nonexistent-llmenv-test-path-12345-test");

    let assert = llmenv_cmd(missing)
        .arg("setup")
        .arg("--no-launch")
        .timeout(TIMEOUT)
        .assert()
        .failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "setup should not panic when config dir is missing (status={:?}): {stderr}",
        output.status.code()
    );
}

/// Running `llmenv setup` without `--no-launch` and without a TTY
/// should not hang and should not panic.
#[test]
fn setup_non_interactive_no_flags() {
    let dir = TempDir::new().unwrap();

    let mut cmd = llmenv_cmd(dir.path());
    cmd.arg("setup").write_stdin("").timeout(TIMEOUT);

    let assert = cmd.assert();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "setup should not panic in non-interactive mode (status={:?}): {stderr}",
        output.status.code()
    );
    // A clear message should be shown (either requesting --no-launch or describing
    // the fallback behaviour)
    assert!(
        !stderr.is_empty() || !output.stdout.is_empty(),
        "setup should produce some output in non-interactive mode"
    );
}
