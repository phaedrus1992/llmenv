#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test scaffolding")]
//! Integration test for `hook-run` delivering `launch`'s pending mid-session
//! notice (#1480) end to end, including the real `LLMENV_LAUNCH_SOCKET`
//! env-var read in `hook_run::launch_client::check_pending_notice`.
//!
//! A unit test can't exercise that env-var read directly — mutating a real
//! process env var is `unsafe` and forbidden workspace-wide (see
//! `src/launch/socket.rs`'s test module for the same constraint). Setting it
//! for a *child* process via `std::process::Command::env` needs no unsafe,
//! so this drives the real `llmenv hook-run` binary instead.
//!
//! Hand-rolls a minimal server for `launch`'s wire protocol (4-byte
//! big-endian length prefix, then JSON; `{"verb":"pending_events"}` request,
//! `{"notice": ...}` response) rather than depending on `src/launch/socket.rs`
//! directly — that module is `pub(crate)`, not reachable from an external
//! integration test crate.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::time::Duration;

use tempfile::TempDir;

mod support;

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

/// A config with `read_once` in warn mode — the same "second read produces a
/// non-empty additionalContext" path `hook_run_failsoft.rs` already relies
/// on, needed here so the notice-append's newline-separator branch (the
/// mutation this test exists to catch) is actually exercised with non-empty
/// existing text.
fn config_with_read_once_warn() -> String {
    format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

bundle:
  - name: test-bundle
    when: [test]

features:
  read_once:
    enabled: true
    mode: warn
    ttl_seconds: 1200

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
    )
}

/// Serve exactly one `pending_events` request with `notice`, then stop.
/// Runs on a background thread; the caller joins it after the hook-run
/// invocation completes.
fn serve_one_notice(listener: UnixListener, notice: &'static str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).is_err() {
            return;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        if stream.read_exact(&mut buf).is_err() {
            return;
        }
        let response = serde_json::json!({ "notice": notice });
        let payload = serde_json::to_vec(&response).unwrap();
        let response_len = u32::try_from(payload.len()).unwrap();
        let _ = stream.write_all(&response_len.to_be_bytes());
        let _ = stream.write_all(&payload);
    })
}

#[test]
fn hook_run_delivers_launch_notice_joined_with_existing_context() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, config_with_read_once_warn()).unwrap();

    let socket_path = dir.path().join("launch.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    // Deliberately not joined: if the client under test never connects (e.g.
    // a mutant that makes `check_pending_notice` return early without
    // dialing the socket), this thread blocks in `accept()` forever. The
    // test's own assertions below — bounded by each `hook-run` subprocess's
    // own timeout — are what actually catches that; waiting on this thread
    // too would just hang the test alongside it. It's reclaimed when the
    // test binary process exits.
    let _server = serve_one_notice(listener, "credentials expire soon");

    let test_file_dir = TempDir::new().unwrap();
    let file_path = test_file_dir.path().join("hook_run_launch_notice.txt");
    fs::write(&file_path, b"content").unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "test-launch-notice",
        "tool_name": "Read",
        "tool_input": { "filePath": file_path.to_str().unwrap() },
    })
    .to_string();

    // First read: passes through empty, establishing read_once's "already
    // read" state for the second call below — same two-call pattern
    // `hook_run_failsoft.rs::pre_tool_use_read_twice_warn_mode` uses.
    let mut first = support::isolated_llmenv_cmd(dir.path());
    first
        .env("LLMENV_CONFIG", &config_path)
        .arg("hook-run")
        .arg("pre_tool_use")
        .write_stdin(payload.as_str());
    first.timeout(Duration::from_secs(10)).assert().success();

    // Second read: read_once's "already read" advisory gives non-empty
    // existing text, so the notice must be appended with a leading newline
    // rather than run together with it.
    let mut second = support::isolated_llmenv_cmd(dir.path());
    second
        .env("LLMENV_CONFIG", &config_path)
        .env("LLMENV_LAUNCH_SOCKET", &socket_path)
        // #1484: the client now requires a token alongside the socket path;
        // this test's hand-rolled server doesn't validate it, so any value
        // that's actually set is enough to make the client dial the socket.
        .env("LLMENV_LAUNCH_TOKEN", "test-token")
        .arg("hook-run")
        .arg("pre_tool_use")
        .write_stdin(payload.as_str());
    let output = second.timeout(Duration::from_secs(10)).output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("hook output must be valid JSON");
    let ctx = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        ctx.contains("already read"),
        "must still carry the read_once advisory; got: {ctx}"
    );
    assert!(
        ctx.contains("credentials expire soon"),
        "must carry the launch notice fetched over LLMENV_LAUNCH_SOCKET; got: {ctx}"
    );
    assert_eq!(
        ctx.lines().last(),
        Some("credentials expire soon"),
        "the notice must be its own trailing line, joined by a newline rather \
         than run together with the read_once advisory; got: {ctx}"
    );
}
