#![expect(clippy::expect_used, reason = "test scaffolding")]
//! `llmenv doctor` sends a real MCP `initialize` to each managed server (#2358).

mod support;

use std::fs;
use std::path::Path;

use support::isolated_llmenv_cmd;

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

fn config() -> String {
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

features:
  codebase_memory:
    - when: [test]

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = user()
    )
}

/// Run `llmenv doctor` with a fake `codebase-memory-mcp` (a shell body) first on `PATH`.
/// Returns what doctor printed on stderr.
#[cfg(unix)]
fn doctor_with_fake_cbm(body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, config()).expect("write config");
    let bin = tempfile::TempDir::new().expect("temp dir");
    let script = bin.path().join("codebase-memory-mcp");
    fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    let old_path = std::env::var("PATH").unwrap_or_default();
    let output = isolated_llmenv_cmd(Path::new(dir.path()))
        .env("LLMENV_CONFIG", &config_path)
        .env("PATH", format!("{}:{old_path}", bin.path().display()))
        .arg("doctor")
        .output()
        .expect("run llmenv doctor");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
#[test]
fn doctor_flags_a_codebase_memory_server_that_does_not_answer() {
    // `--version` is doctor's own tool check; only the MCP handshake hangs.
    let stderr = doctor_with_fake_cbm("case \"$1\" in --*) exit 0;; esac\nexec sleep 30");
    assert!(stderr.contains("MCP servers:"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("codebase-memory-mcp failed MCP initialize"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("cbm-daemon-internal"), "stderr:\n{stderr}");
}

#[cfg(unix)]
#[test]
fn doctor_passes_a_codebase_memory_server_that_answers() {
    let reply = r#"{"jsonrpc":"2.0","id":0,"result":{}}"#;
    let stderr = doctor_with_fake_cbm(&format!("read line\nprintf '%s\\n' '{reply}'"));
    assert!(
        stderr.contains("codebase-memory-mcp answers MCP initialize"),
        "stderr:\n{stderr}"
    );
}
