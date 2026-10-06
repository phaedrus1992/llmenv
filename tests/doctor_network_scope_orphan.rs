#![expect(clippy::expect_used, reason = "test scaffolding")]
//! Doctor's orphan detection flags a network scope whose `match` sets no field, because such a
//! scope can never activate. A scope with `ssid` or `cidr` is matchable (#1051).

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

#[test]
fn doctor_all_flags_network_scope_with_an_empty_match() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = r#"
scope:
  network:
    - id: home
      match: {}
      tags: [home]
  host: []
  user: []
cache:
  cache_dir: ~/.cache/llmenv
  cache_retention_hours: 168
capabilities:
  hooks: []
bundle: []
mcp: []
plugin_marketplace: []
plugin_collection: []
"#;
    fs::write(tmp.path().join("config.yaml"), config).expect("write config");

    let output = isolated_llmenv_cmd(tmp.path())
        .args(["doctor", "--all"])
        .output()
        .expect("run llmenv doctor --all");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("network:home: match sets none of gateway_mac, ssid, or cidr"),
        "expected a warning naming the network:home scope, got: {stderr}"
    );
}

#[test]
fn doctor_all_does_not_flag_network_scope_with_gateway_mac() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = r#"
scope:
  network:
    - id: office
      match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }
      tags: [office]
  host: []
  user: []
cache:
  cache_dir: ~/.cache/llmenv
  cache_retention_hours: 168
capabilities:
  hooks: []
bundle: []
mcp: []
plugin_marketplace: []
plugin_collection: []
"#;
    fs::write(tmp.path().join("config.yaml"), config).expect("write config");

    let output = isolated_llmenv_cmd(tmp.path())
        .args(["doctor", "--all"])
        .output()
        .expect("run llmenv doctor --all");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Doctor check complete"),
        "doctor must have run to completion, got: {stderr}"
    );
    assert!(
        !stderr.contains("network:office: match sets none of"),
        "must not flag a scope that already has gateway_mac set, got: {stderr}"
    );
}

#[test]
fn doctor_all_does_not_flag_network_scopes_with_ssid_or_cidr() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = r#"
scope:
  network:
    - id: home
      match: { ssid: "MyHomeWifi" }
      tags: [home]
    - id: lab
      match: { cidr: "10.20.0.0/16" }
      tags: [lab]
  host: []
  user: []
cache:
  cache_dir: ~/.cache/llmenv
  cache_retention_hours: 168
capabilities:
  hooks: []
bundle: []
mcp: []
plugin_marketplace: []
plugin_collection: []
"#;
    fs::write(tmp.path().join("config.yaml"), config).expect("write config");

    let output = isolated_llmenv_cmd(tmp.path())
        .args(["doctor", "--all"])
        .output()
        .expect("run llmenv doctor --all");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Doctor check complete"), "got: {stderr}");
    assert!(
        !stderr.contains("match sets none of"),
        "ssid and cidr scopes are matchable, got: {stderr}"
    );
    // A config with an `ssid` scope makes doctor report what this machine can read.
    assert!(stderr.contains("Network scopes:"), "got: {stderr}");
}
