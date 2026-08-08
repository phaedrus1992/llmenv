#![expect(clippy::expect_used, reason = "test scaffolding")]
//! #756: `llmenv completions --install` writes the shell's completion script
//! to its standard directory instead of just printing to stdout.

use assert_cmd::Command;
use std::fs;

fn llmenv_cmd() -> Command {
    Command::cargo_bin("llmenv").expect("find llmenv binary")
}

#[test]
fn install_writes_bash_completion_to_custom_dir() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let install_dir = tempfile::TempDir::new().expect("tempdir");

    llmenv_cmd()
        .env("HOME", home.path())
        .arg("completions")
        .arg("bash")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .assert()
        .success();

    let script = install_dir.path().join("llmenv");
    assert!(script.exists(), "expected completion script at {script:?}");
    let content = fs::read_to_string(&script).expect("read script");
    assert!(
        content.contains("llmenv"),
        "completion script should reference the binary name"
    );
}

#[test]
fn install_writes_zsh_completion_with_underscore_prefix() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let install_dir = tempfile::TempDir::new().expect("tempdir");

    llmenv_cmd()
        .env("HOME", home.path())
        .arg("completions")
        .arg("zsh")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .assert()
        .success();

    assert!(install_dir.path().join("_llmenv").exists());
}

#[test]
fn install_refuses_to_overwrite_without_force() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let install_dir = tempfile::TempDir::new().expect("tempdir");

    llmenv_cmd()
        .env("HOME", home.path())
        .arg("completions")
        .arg("bash")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .assert()
        .success();
    let output = llmenv_cmd()
        .env("HOME", home.path())
        .arg("completions")
        .arg("bash")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .output()
        .expect("run install again");
    assert!(
        !output.status.success(),
        "second install without --force should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--force"),
        "error should mention --force; got: {stderr}"
    );
}

#[test]
fn install_overwrites_with_force() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let install_dir = tempfile::TempDir::new().expect("tempdir");
    let script = install_dir.path().join("llmenv");
    fs::write(&script, "stale content").expect("seed stale file");

    llmenv_cmd()
        .env("HOME", home.path())
        .arg("completions")
        .arg("bash")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .arg("--force")
        .assert()
        .success();

    let content = fs::read_to_string(&script).expect("read script");
    assert_ne!(content, "stale content", "must overwrite with --force");
}

#[test]
fn install_without_shell_arg_uses_shell_env() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let install_dir = tempfile::TempDir::new().expect("tempdir");

    llmenv_cmd()
        .env("HOME", home.path())
        .env("SHELL", "/bin/zsh")
        .arg("completions")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .assert()
        .success();

    assert!(
        install_dir.path().join("_llmenv").exists(),
        "expected zsh completion detected from $SHELL"
    );
}

#[test]
fn install_without_shell_arg_or_shell_env_fails_clearly() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let install_dir = tempfile::TempDir::new().expect("tempdir");

    let output = llmenv_cmd()
        .env("HOME", home.path())
        .env_remove("SHELL")
        .arg("completions")
        .arg("--install")
        .arg("--dir")
        .arg(install_dir.path())
        .output()
        .expect("run install");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SHELL") || stderr.contains("--shell"),
        "error should mention $SHELL or --shell; got: {stderr}"
    );
}

#[test]
fn plain_completions_still_prints_to_stdout() {
    // Non-regression: the pre-#756 behavior (no --install) must keep working.
    let output = llmenv_cmd()
        .arg("completions")
        .arg("bash")
        .output()
        .expect("run completions");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("llmenv"),
        "stdout should contain the generated completion script"
    );
}
