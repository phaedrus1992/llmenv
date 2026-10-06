#![expect(clippy::expect_used, reason = "test scaffolding")]
//! A cleartext `http://` MCP URL on a public IP literal fails the config render (#2483).

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

/// Write a config with one firing bundle (so the MCP resolver runs) and one MCP server.
fn setup(url: &str) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bundle = dir.path().join("bundles/base");
    fs::create_dir_all(&bundle).expect("mkdir bundle");
    fs::write(bundle.join("bundle.yaml"), "hooks: []\n").expect("write bundle");
    fs::write(bundle.join("AGENTS.md"), "# base\n").expect("write agents");
    let config = format!(
        r#"
scope:
  user:
    - id: u
      match:
        user: {user}
      tags: [test]
tag:
  test: ""
mcp:
  - name: ctx7
    when: [test]
    type: http
    url: {url}
bundle:
  - name: base
    when: [test]
cache:
  sync_interval_minutes: 60
adapter:
  engine: claude-code
"#,
        user = user()
    );
    fs::write(dir.path().join("config.yaml"), config).expect("write config");
    dir
}

fn run(dir: &tempfile::TempDir, sub: &str) -> (bool, String) {
    let output = isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", dir.path().join("config.yaml"))
        .arg(sub)
        .output()
        .expect("run llmenv");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn export_and_doctor_refuse_http_to_a_public_ip() {
    let dir = setup("http://93.184.216.34/mcp");
    for sub in ["export", "doctor"] {
        let (ok, stderr) = run(&dir, sub);
        assert!(!ok, "{sub} must fail:\n{stderr}");
        assert!(stderr.contains("ctx7"), "{sub}:\n{stderr}");
        assert!(stderr.contains("https://"), "{sub}:\n{stderr}");
    }
}

#[test]
fn export_accepts_https_to_a_public_ip() {
    let dir = setup("https://93.184.216.34/mcp");
    let (ok, stderr) = run(&dir, "export");
    assert!(ok, "export must pass:\n{stderr}");
}

#[test]
fn export_accepts_http_to_a_private_ip() {
    let dir = setup("http://10.0.0.4/mcp");
    let (ok, stderr) = run(&dir, "export");
    assert!(ok, "export must pass:\n{stderr}");
}
