#![expect(clippy::expect_used, reason = "test scaffolding")]
//! Test for #1051: doctor's orphan detection flags a network scope whose
//! `match` has `ssid`/`cidr` but no `gateway_mac` -- the matcher only
//! evaluates `gateway_mac` today, so such a scope can never activate.

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

#[test]
fn doctor_all_flags_network_scope_with_only_ssid() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = r#"
scope:
  network:
    - id: home
      match: { ssid: "MyHomeWifi" }
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
        stderr.contains("network:home: match has no gateway_mac"),
        "expected a warning naming the network:home scope and gateway_mac, got: {stderr}"
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
        !stderr.contains("network:office: match has no gateway_mac"),
        "must not flag a scope that already has gateway_mac set, got: {stderr}"
    );
}
