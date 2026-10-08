#![expect(clippy::expect_used, reason = "test scaffolding")]
//! A CLI command whose reader exits early must not abort (#2554).
//!
//! `llmenv task ls | head` closes stdout before the command finishes writing. The write fails
//! with `Broken pipe`, and before the panic hook that failure ended the process in `SIGABRT`.
//! This test spawns the real binary, closes the read end of its stdout before it writes, and
//! checks that it exits with the closed-pipe status and prints no panic text.

use std::process::{Command, Stdio};

use tempfile::TempDir;

/// 128 plus SIGPIPE (13). The status the panic hook exits with on a closed stdout.
const CLOSED_PIPE_STATUS: i32 = 141;

#[test]
fn closed_stdout_exits_quietly_without_abort() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("config.yaml"), "{}\n").expect("write config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_llmenv"))
        .args(["task", "ls", "--all", "--format", "json"])
        .env("LLMENV_CONFIG_DIR", dir.path())
        .env("LLMENV_STATE_DIR", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env("XDG_CACHE_HOME", dir.path())
        .env("HOME", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn llmenv");

    // Drop the read end before the child writes, so its first write gets EPIPE.
    drop(child.stdout.take());
    let status = child.wait().expect("wait for llmenv");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("stderr"), &mut stderr)
        .expect("read stderr");

    assert_eq!(
        status.code(),
        Some(CLOSED_PIPE_STATUS),
        "expected a quiet exit, got {status:?}; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "panic text on stderr: {stderr}"
    );
}
