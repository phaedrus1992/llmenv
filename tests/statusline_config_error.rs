//! #1052: a `config.yaml` that won't parse must not blank the statusline.
//!
//! The statusline is rendered by the agent on every prompt and its stderr is
//! discarded, so an error that only reaches stderr is invisible. These tests
//! pin the observable contract: exit 0, and a visible error row on stdout.
#![expect(clippy::unwrap_used, reason = "test scaffolding")]
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A config dir whose `config.yaml` holds `content`.
fn config_dir_with(content: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("config.yaml"), content).unwrap();
    dir
}

fn statusline(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG_DIR", dir.path())
        .arg("statusline")
        .write_stdin("{}");
    cmd
}

#[test]
fn unparseable_config_renders_an_error_row_instead_of_nothing() {
    // `rows:` wants a sequence; a mapping is a parse error.
    let dir = config_dir_with("statusline:\n  rows:\n    nope: 1\n");
    statusline(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("config error"))
        .stdout(predicate::str::contains("llmenv doctor"));
}

#[test]
fn malformed_yaml_config_renders_an_error_row() {
    let dir = config_dir_with("scope: [unclosed\n");
    statusline(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("config error"));
}

#[test]
fn valid_config_renders_widgets_not_the_error_row() {
    // Guards against the error row becoming unconditional.
    let dir = config_dir_with("statusline:\n  rows:\n    - \"{model}\"\n");
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG_DIR", dir.path())
        .arg("statusline")
        .write_stdin(r#"{"model":{"display_name":"Opus 4.8"}}"#);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Opus"))
        .stdout(predicate::str::contains("config error").not());
}
