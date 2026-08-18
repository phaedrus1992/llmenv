#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test scaffolding")]
//! Integration tests for `llmenv edit`'s signal supervision (#1385).
//!
//! `edit` hands the terminal to `$EDITOR` and waits. It used to wait under the
//! default signal disposition, so a SIGINT delivered to `llmenv` alone — which
//! is what a supervisor, a script, or `kill` does, as opposed to the terminal's
//! group-wide Ctrl-C — killed `llmenv` and left the editor running against the
//! config file with the shell drawing a prompt over it.
//!
//! Uses `tests/fixtures/fake_engine.sh` as the editor: it writes a marker before
//! sleeping, so a test can wait for "the editor is really up" instead of racing
//! a fixed delay, then exits with a chosen code.

use std::fs;
use std::time::Duration;

/// Send `signal` (a `kill(1)` name like `INT`) to `pid`. Deliberately a local
/// copy of `tests/launch.rs`'s helper rather than a shared one in
/// `tests/support`: an item there that only some test binaries use trips
/// `dead_code` in the rest, and the workspace forbids bare `allow` attributes.
#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("send signal to llmenv");
    assert!(status.success(), "kill -{signal} should have succeeded");
}

/// Hang detector, not a performance assertion — matches `tests/launch.rs`.
const EDIT_TIMEOUT_SECS: u64 = 30;

/// How long the fake editor stays up. Long enough that `llmenv` dying on the
/// signal instead of waiting is unambiguous in the elapsed time.
const EDITOR_SLEEP_SECS: u64 = 2;

/// Copy `fake_engine.sh` into `dir` as an executable `fake-editor`, returning
/// its path. Copied rather than referenced in place so `$EDITOR` is a path under
/// the test's own tempdir (`run_edit` splits `$EDITOR` on whitespace, so a
/// checkout path containing a space would not survive).
fn install_fake_editor(dir: &std::path::Path) -> std::path::PathBuf {
    let src =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_engine.sh");
    let dest = dir.join("fake-editor");
    fs::copy(&src, &dest).expect("copy fake editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).expect("chmod fake editor");
    }
    dest
}

/// A tempdir doubling as `LLMENV_CONFIG_DIR`, holding a `config.yaml` for
/// `edit` to open.
fn setup_config_dir() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    fs::write(
        dir.path().join("config.yaml"),
        "adapter:\n  engine: claude-code\n",
    )
    .unwrap();
    dir
}

/// `llmenv edit` as a plain `std::process::Command` so the test can hold the
/// child and signal it mid-run; `assert_cmd` runs to completion, which is
/// exactly what these tests must not do.
#[cfg(unix)]
fn edit_std_cmd(dir: &std::path::Path, editor: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("llmenv"));
    for key in [
        "LLMENV_CONFIG_DIR",
        "LLMENV_STATE_DIR",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "HOME",
    ] {
        cmd.env(key, dir);
    }
    cmd.env("EDITOR", editor);
    cmd.env_remove("VISUAL");
    cmd.arg("edit");
    cmd
}

/// SIGINT delivered to `llmenv edit` alone must not end it: the editor is still
/// on the terminal, so `llmenv` has to keep waiting and report the editor's own
/// outcome. Before #1385 this returned immediately with a signal-derived status
/// and left the editor running.
#[test]
#[cfg(unix)]
fn sigint_does_not_orphan_the_editor() {
    let dir = setup_config_dir();
    let editor = install_fake_editor(dir.path());
    let ready_marker = dir.path().join("editor_started.txt");

    let mut cmd = edit_std_cmd(dir.path(), &editor);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &ready_marker);
    cmd.env("FAKE_ENGINE_SLEEP_SECS", EDITOR_SLEEP_SECS.to_string());
    let mut child = cmd.spawn().expect("spawn llmenv edit");

    let waited_from = std::time::Instant::now();
    while !ready_marker.exists() {
        assert!(
            waited_from.elapsed() < Duration::from_secs(EDIT_TIMEOUT_SECS),
            "fake editor never started under `llmenv edit`"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let start = std::time::Instant::now();
    send_signal(child.id(), "INT");
    let status = child.wait().expect("wait on llmenv edit");
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(1200),
        "llmenv should have kept waiting for the editor's remaining {EDITOR_SLEEP_SECS}s \
         rather than exiting on the SIGINT; took {elapsed:?} with {status:?}"
    );
    assert!(
        status.success(),
        "the editor exited 0, so llmenv should too; got {status:?}"
    );
}

/// The other half of supervision: the status `edit` reports is the editor's, not
/// llmenv's own.
#[test]
#[cfg(unix)]
fn editor_failure_is_reported_with_the_editors_status() {
    let dir = setup_config_dir();
    let editor = install_fake_editor(dir.path());

    let mut cmd = edit_std_cmd(dir.path(), &editor);
    cmd.env("FAKE_ENGINE_EXIT_CODE", "7");
    cmd.stderr(std::process::Stdio::piped());
    let out = cmd.output().expect("run llmenv edit");

    assert!(!out.status.success(), "a failing editor must fail `edit`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("editor exited with"),
        "stderr should name the editor's exit, got: {stderr}"
    );
    assert!(
        stderr.contains('7'),
        "stderr should carry the editor's own exit code 7, got: {stderr}"
    );
}
