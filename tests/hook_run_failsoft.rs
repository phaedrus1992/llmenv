#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test scaffolding")]
//! Integration tests for the fail-soft contract of `llmenv hook-run <event>` (#187).
//!
//! Lifecycle hooks run on the agent's hot path — session start and every prompt
//! turn. The dispatcher's hard guarantee is: **it never blocks the agent.** No
//! matter what goes wrong (bad event name, no backend, malformed/SSRF-rejected
//! URL, unreachable host), the command must exit 0, emit nothing on stdout, and
//! degrade to a single `llmenv:`-prefixed warning on stderr.
//!
//! These tests drive the compiled binary (via `assert_cmd`) so they exercise the
//! real `run()` entry point end to end, including config resolution and the SSRF
//! URL guard. Backend-response parsing (JSON-RPC error bodies, missing/malformed
//! `result.content`) is covered by unit tests in `src/hook_run/mcp_client.rs`,
//! which can use the test-only client to point at a wiremock server. The
//! production CLI path's `McpHttpClient::new` allows loopback/private ranges
//! too (`SsrfPolicy::AllowPrivateNetwork`) — that's the expected topology for
//! llmenv's own ICM backend (AGENTS.md) — so these tests exercise that
//! directly rather than needing the test-only client's bypass.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

/// Current OS user, used to make a user scope match in test configs.
fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

/// Write `config.yaml` into a fresh temp dir and return the dir (kept alive for
/// the test) plus its path. The dir doubles as `LLMENV_CONFIG_DIR`.
fn setup_config(content: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, content).unwrap();
    (dir, config_path)
}

/// A valid config whose active user scope carries tag `test`, with no `memory:`
/// backend. The hook resolves a scope but finds no memory MCP to talk to.
fn config_with_read_once(mode: &str) -> String {
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
    mode: {mode}
    ttl_seconds: 1200
    diff: false

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
        mode = mode,
    )
}

fn config_no_backend() -> String {
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

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
    )
}

/// A valid config whose memory backend resolves to `http://{addr}:{port}`.
/// The memory topology is the only path to a resolved backend URL — a plain
/// `mcp:` entry named `icm` is a reserved-name validation error. `addr` is the
/// `host:` table entry the `memory.server_host` points at, so it controls the
/// host portion of the resolved URL; `port` controls the port.
fn config_with_memory_addr(addr: &str, port: u16) -> String {
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

host:
  memhost:
    addr: "{addr}"

features:
  memory:
    - server_host: memhost
      port: {port}
      when: [test]

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
        addr = addr,
        port = port,
    )
}

/// Build a `Command` for `llmenv hook-run <event>` pointed at the temp config
/// dir, with HOME-derived state redirected so the test never touches real config.
fn hook_cmd(config_dir: &std::path::Path, config_path: &std::path::Path, event: &str) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_STATE_DIR", config_dir)
        .arg("hook-run")
        .arg(event);
    cmd
}

/// Assert the fail-soft contract: exit 0, empty stdout, and a stderr warning
/// containing `stderr_needle`.
fn assert_fail_soft(mut cmd: Command, stderr_needle: &str) {
    cmd.timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(stderr_needle));
}

#[test]
fn unknown_event_exits_zero_with_warning() {
    // The event name is rejected before any config load, so a near-empty config
    // is fine here.
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, "adapter:\n  engine: claude-code\n").unwrap();

    assert_fail_soft(
        hook_cmd(dir.path(), &config_path, "not_a_real_event"),
        "unknown hook event",
    );
}

#[test]
fn no_memory_backend_active_exits_zero_with_warning() {
    // Valid config, active scope, but no `memory:` topology — nothing to recall.
    let (dir, config_path) = setup_config(&config_no_backend());

    assert_fail_soft(
        hook_cmd(dir.path(), &config_path, "session_start"),
        "no memory backend active",
    );
}

#[test]
fn malformed_backend_url_exits_zero_with_warning() {
    // A host addr that can't be DNS-resolved (`http://no-such-host.invalid:9`)
    // must fail-soft at client construction, not panic. The `.invalid` TLD is
    // reserved by RFC 2606 and guaranteed to never resolve.
    let (dir, config_path) = setup_config(&config_with_memory_addr("no-such-host.invalid", 9));

    assert_fail_soft(
        hook_cmd(dir.path(), &config_path, "session_start"),
        "invalid memory backend URL",
    );
}

#[test]
fn loopback_url_is_allowed_not_ssrf_rejected() {
    // Loopback is the expected topology for llmenv's own same-host ICM backend
    // (AGENTS.md) and must NOT be SSRF-rejected — `McpHttpClient::new` uses
    // `SsrfPolicy::AllowPrivateNetwork`. Nothing listens on this discard port,
    // so the *connection* fails instead, but that's a plain unreachable-backend
    // fail-soft ("skipped"), not an "invalid ... SSRF" rejection.
    let (dir, config_path) = setup_config(&config_with_memory_addr("127.0.0.1", 9));

    assert_fail_soft(
        hook_cmd(dir.path(), &config_path, "session_start"),
        "session_start skipped",
    );
}

#[test]
fn private_network_url_is_allowed_not_ssrf_rejected() {
    // Private-range IPs are likewise the expected LAN topology for a remote
    // `icm serve` (AGENTS.md) and must not be SSRF-rejected.
    let (dir, config_path) = setup_config(&config_with_memory_addr("10.0.0.1", 8080));

    assert_fail_soft(
        hook_cmd(dir.path(), &config_path, "session_start"),
        "session_start skipped",
    );
}

#[test]
fn unreachable_public_backend_exits_zero_with_warning() {
    // A syntactically valid, SSRF-allowed public URL that nothing is listening
    // on. The HTTP round-trip fails; the dispatcher must still exit 0. Uses the
    // TEST-NET-1 documentation range (192.0.2.0/24, RFC 5737) on the discard
    // port so it fails fast within the 2s hook timeout without touching a real
    // host.
    let (dir, config_path) = setup_config(&config_with_memory_addr("192.0.2.1", 9));

    assert_fail_soft(
        hook_cmd(dir.path(), &config_path, "session_start"),
        "session_start skipped",
    );
}

#[test]
fn all_events_fail_soft_without_backend() {
    // The fail-soft contract holds for every lifecycle event, not just one.
    let (dir, config_path) = setup_config(&config_no_backend());

    for event in ["session_start", "turn_start", "session_end", "pre_tool_use"] {
        hook_cmd(dir.path(), &config_path, event)
            .timeout(Duration::from_secs(10))
            .assert()
            .success()
            .stdout(predicate::str::is_empty());
    }
}

#[test]
fn pre_tool_use_without_read_once_config_passes_through() {
    let (dir, config_path) = setup_config(&config_no_backend());
    hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn pre_tool_use_with_read_once_warn_config_passes_through() {
    let (dir, config_path) = setup_config(&config_with_read_once("warn"));
    hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn pre_tool_use_with_read_once_deny_config_passes_through() {
    let (dir, config_path) = setup_config(&config_with_read_once("deny"));
    hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/// Helper: a synthetic PreToolUse payload for a Read tool call.
fn read_hook_payload(file_path: &str, session_id: &str) -> String {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": session_id,
        "tool_name": "Read",
        "tool_input": {
            "filePath": file_path,
        },
    })
    .to_string()
}

#[test]
fn pre_tool_use_read_twice_warn_mode() {
    let (dir, config_path) = setup_config(&config_with_read_once("warn"));

    // Create a real file in its own temp dir so both subprocess calls see it.
    let test_file_dir = TempDir::new().unwrap();
    let file_path = test_file_dir.path().join("read_twice_warn.txt");
    fs::write(&file_path, b"content for warn mode test").unwrap();

    let session_id = "test-warn-twice";
    let payload = read_hook_payload(file_path.to_str().unwrap(), session_id);

    // First read — passes through, empty stdout
    hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .write_stdin(payload.as_str())
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // Second read — warns, non-empty stdout with advisory JSON
    let output = hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .write_stdin(payload.as_str())
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "warn mode second read should exit 0"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.is_empty(),
        "warn mode second read should produce output"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("warn mode output should be valid JSON");
    let ctx = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("");
    assert!(
        ctx.contains("already read"),
        "warn mode advisory should mention re-read; got: {ctx}"
    );
}

#[test]
fn pre_tool_use_read_twice_deny_mode() {
    let (dir, config_path) = setup_config(&config_with_read_once("deny"));

    let test_file_dir = TempDir::new().unwrap();
    let file_path = test_file_dir.path().join("read_twice_deny.txt");
    fs::write(&file_path, b"content for deny mode test").unwrap();

    let session_id = "test-deny-twice";
    let payload = read_hook_payload(file_path.to_str().unwrap(), session_id);

    // First read — passes through, empty stdout
    hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .write_stdin(payload.as_str())
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // Second read — denied, stdout should be a deny JSON envelope
    let output = hook_cmd(dir.path(), &config_path, "pre_tool_use")
        .write_stdin(payload.as_str())
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "deny mode second read should exit 0"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.is_empty(),
        "deny mode second read should produce output"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("deny mode output should be valid JSON");
    let deny = &parsed["hookSpecificOutput"];
    assert_eq!(
        deny["permissionDecision"].as_str(),
        Some("deny"),
        "should have permissionDecision=deny"
    );
    assert_eq!(
        deny["hookEventName"].as_str(),
        Some("PreToolUse"),
        "should have hookEventName=PreToolUse"
    );
    let reason = deny["deniedReason"].as_str().unwrap_or("");
    assert!(
        reason.contains("already read"),
        "deny reason should mention re-read; got: {reason}"
    );
}
