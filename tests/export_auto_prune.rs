#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! `llmenv export` runs the `auto_prune` pass, and the shell evaluates the stdout of `export`.
//! The prune summary must therefore stay out of stdout (#2947).

use std::time::Duration;

use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support;

fn reply(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {"content": [{"type": "text", "text": text}]}
    }))
}

fn config(port: u16, cache_dir: &std::path::Path) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "runner".to_string());
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
    addr: "127.0.0.1"
features:
  memory:
    - server_host: memhost
      port: {port}
      when: [test]
      auto_prune: true
cache:
  cache_dir: "{cache}"
  sync_interval_minutes: 60
adapter:
  engine: claude-code
"#,
        cache = cache_dir.display()
    )
}

#[test]
fn export_keeps_the_prune_summary_out_of_stdout() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("icm_memory_recall"))
            .respond_with(reply(
                r#"[{"id":"old-1","topic":"t","summary":"s","importance":"low","weight":0.1}]"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(reply(""))
            .mount(&server)
            .await;
        server
    });
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        config(server.address().port(), &dir.path().join("cache")),
    )
    .unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status()
        .unwrap();
    assert!(init.success());

    let out = support::isolated_llmenv_cmd(dir.path())
        .env("LLMENV_CONFIG", &config_path)
        .current_dir(&project)
        .arg("export")
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Prune"), "{stdout}");
    assert!(!stdout.contains("record(s)"), "{stdout}");

    let forgot = rt.block_on(server.received_requests()).unwrap();
    assert!(
        forgot
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains("icm_memory_forget")),
        "the prune ran and forgot the low record"
    );
}
