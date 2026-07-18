#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! End-to-end smoke tests for llmenv CLI across realistic config combinations.
//!
//! These tests exercise the compiled binary against a matrix of representative
//! configurations — multiple adapters, feature toggles, and MCP/memory backends —
//! driving it through real entry points (`export`, `regenerate`, `hook-run` for
//! each lifecycle event). The primary goal is to catch hangs, timeouts, and
//! config-related failures that unit tests don't surface.
//!
//! Motivation: #543 (Crush materialization hang), #547 (DNS resolution hang),
//! #548 (ICM memory backend crash), and the general pattern of hangs/timeouts
//! caught post-facto in production. This suite runs on every CI pass to prevent
//! regression.

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

/// Base config scaffold: minimal valid config for the given adapter + scope.
fn config_base(adapter: &str) -> String {
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

cache:
  sync_interval_minutes: 60

adapter:
  engine: {adapter}
"#,
        user = current_user(),
        adapter = adapter,
    )
}

/// Config with memory backend enabled (pointing at an unreachable address).
/// Used to test that timeout/failsoft works for backend connection failures.
fn config_with_memory(adapter: &str, addr: &str, port: u16) -> String {
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
  engine: {adapter}
"#,
        user = current_user(),
        addr = addr,
        port = port,
        adapter = adapter,
    )
}

/// Config with a bundle definition declared in the `bundle:` section, so
/// `build_manifest` receives non-empty refs and exercises the merged-manifest
/// code path (#708, #830).  The bundle directory must be created on disk at
/// `{config_dir}/bundles/{name}/bundle.yaml` before running the command.
fn config_with_bundle(adapter: &str, bundle_name: &str) -> String {
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
  - name: {bundle}
    when: [test]

cache:
  sync_interval_minutes: 60

adapter:
  engine: {adapter}
"#,
        user = current_user(),
        bundle = bundle_name,
        adapter = adapter,
    )
}

/// Config with a `codebase_memory` entry plus a bundle so `build_manifest`
/// receives non-empty refs (its materialization code path — where
/// `codebase_memory` resolution is wired — early-returns `None` otherwise,
/// same as the bundle-gating `config_with_bundle` needs for memory).
fn config_with_codebase_memory(adapter: &str, bundle_name: &str) -> String {
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
  - name: {bundle}
    when: [test]

features:
  codebase_memory:
    - when: [test]

cache:
  sync_interval_minutes: 60

adapter:
  engine: {adapter}
"#,
        user = current_user(),
        bundle = bundle_name,
        adapter = adapter,
    )
}

/// Build a `Command` for `llmenv <subcommand>` pointed at the temp config.
fn llmenv_cmd(
    config_dir: &std::path::Path,
    config_path: &std::path::Path,
    subcommand: &str,
) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_STATE_DIR", config_dir);

    // Handle subcommands with args (e.g., "hook-run session_start" -> two args)
    for part in subcommand.split_whitespace() {
        cmd.arg(part);
    }
    cmd
}

/// Run a command with an explicit timeout and assert it completes within that time.
/// Returns the assert result for further validation.
fn assert_completes_within(mut cmd: Command, timeout_secs: u64) -> assert_cmd::assert::Assert {
    let timeout = Duration::from_secs(timeout_secs);
    cmd.timeout(timeout).assert()
}

// ============================================================================
// Test Cases: Config Matrix
// ============================================================================

#[test]
fn smoke_claude_code_basic_export() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "export");
    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains("export "));
}

#[test]
fn smoke_crush_basic_export() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "export");
    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains("export "));
}

#[test]
fn smoke_claude_code_basic_regenerate() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "regenerate");
    assert_completes_within(cmd, 10).success();
}

#[test]
fn smoke_crush_basic_regenerate() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "regenerate");
    assert_completes_within(cmd, 10).success();
}

// ============================================================================
// Test Cases: Hook Events
// ============================================================================

#[test]
fn smoke_claude_code_hook_session_start() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_claude_code_hook_turn_start() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run turn_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_claude_code_hook_session_end() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_end");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_crush_hook_session_start() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_crush_hook_turn_start() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run turn_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_crush_hook_session_end() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_end");
    assert_completes_within(cmd, 5).success();
}

// ============================================================================
// Test Cases: Fault Injection (Unreachable Backends)
// ============================================================================

#[test]
fn smoke_claude_code_memory_unreachable_host() {
    // Unreachable host at a valid address (RFC 5737 TEST-NET-1).
    // The memory backend should timeout/failsoft without hanging.
    let (dir, config_path) = setup_config(&config_with_memory("claude-code", "192.0.2.1", 9));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_crush_memory_unreachable_host() {
    let (dir, config_path) = setup_config(&config_with_memory("crush", "192.0.2.1", 9));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_claude_code_memory_invalid_dns() {
    // An invalid hostname that can't be resolved. The `.invalid` TLD is
    // reserved by RFC 2606 and guaranteed to never resolve. This should fail
    // gracefully, not hang.
    let (dir, config_path) = setup_config(&config_with_memory(
        "claude-code",
        "no-such-host.invalid",
        9,
    ));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_crush_memory_invalid_dns() {
    let (dir, config_path) = setup_config(&config_with_memory("crush", "no-such-host.invalid", 9));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_claude_code_memory_loopback_unreachable() {
    // Loopback is allowed (AGENTS.md, ICM same-host topology), but if nothing
    // is listening, the connection should fail gracefully within the timeout.
    let (dir, config_path) = setup_config(&config_with_memory("claude-code", "127.0.0.1", 9));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

#[test]
fn smoke_crush_memory_loopback_unreachable() {
    let (dir, config_path) = setup_config(&config_with_memory("crush", "127.0.0.1", 9));
    let cmd = llmenv_cmd(dir.path(), &config_path, "hook-run session_start");
    assert_completes_within(cmd, 5).success();
}

// ============================================================================
// Test Cases: Doctor (Configuration Validation)
// ============================================================================

#[test]
fn smoke_claude_code_doctor_succeeds() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "doctor");
    assert_completes_within(cmd, 10).success();
}

#[test]
fn smoke_crush_doctor_succeeds() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "doctor");
    assert_completes_within(cmd, 10).success();
}

// ============================================================================
// Test Cases: Status (Current Configuration State)
// ============================================================================

#[test]
fn smoke_claude_code_status_succeeds() {
    let (dir, config_path) = setup_config(&config_base("claude-code"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "status");
    assert_completes_within(cmd, 10)
        .success()
        .stderr(predicate::str::contains("Scopes"));
}

#[test]
fn smoke_crush_status_succeeds() {
    let (dir, config_path) = setup_config(&config_base("crush"));
    let cmd = llmenv_cmd(dir.path(), &config_path, "status");
    assert_completes_within(cmd, 10)
        .success()
        .stderr(predicate::str::contains("Scopes"));
}

// ============================================================================
// Test Cases: Bundle Manifest (Shared-Manifest Code Path, #708 / #830)
// ============================================================================
//
// Unlike the basic-export tests above (which have no `bundle:` section, so
// `build_manifest` early-returns `None`), these tests declare a bundle that
// *fires* for the active `test` tag, causing `build_manifest` to return
// `Some(...)` and exercise the full merge → resolve → throttle pipeline.
// The shared-manifest optimization (#708) builds the manifest once before the
// adapter loop; these tests verify the entire path stays wired.
//
// Unlike other sections, only the `claude-code` adapter is tested here — the
// manifest builder is adapter-agnostic (it runs before the adapter loop), so
// a single adapter exercises the bundle code path. Crush variants would pass
// trivially without adding meaningful coverage.

#[test]
fn smoke_claude_code_export_with_bundle() {
    let (dir, config_path) = setup_config(&config_with_bundle("claude-code", "test-bundle"));
    let bundle_dir = dir.path().join("bundles").join("test-bundle");
    fs::create_dir_all(&bundle_dir).unwrap();
    fs::write(bundle_dir.join("bundle.yaml"), "{}").unwrap();
    let cmd = llmenv_cmd(dir.path(), &config_path, "export");
    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains(
            "LLMENV_ACTIVE_BUNDLES='test-bundle'",
        ));
}

#[test]
fn smoke_claude_code_regenerate_with_bundle() {
    let (dir, config_path) = setup_config(&config_with_bundle("claude-code", "test-bundle"));
    let bundle_dir = dir.path().join("bundles").join("test-bundle");
    fs::create_dir_all(&bundle_dir).unwrap();
    fs::write(bundle_dir.join("bundle.yaml"), "{}").unwrap();
    let cmd = llmenv_cmd(dir.path(), &config_path, "regenerate");
    assert_completes_within(cmd, 10).success();
}

// #365: features.codebase_memory materializes without error (and without
// needing the codebase-memory-mcp binary installed — export only writes the
// resolved stdio command reference into the engine config, it never launches
// the process).
#[test]
fn smoke_claude_code_export_with_codebase_memory() {
    let (dir, config_path) =
        setup_config(&config_with_codebase_memory("claude-code", "test-bundle"));
    let bundle_dir = dir.path().join("bundles").join("test-bundle");
    fs::create_dir_all(&bundle_dir).unwrap();
    fs::write(bundle_dir.join("bundle.yaml"), "{}").unwrap();
    let cmd = llmenv_cmd(dir.path(), &config_path, "export");
    assert_completes_within(cmd, 10).success();
}
