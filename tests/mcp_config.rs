//! Tests for #14: ICM MCP server/client config emission in Claude Code adapter.

use std::path::PathBuf;

use llmenv::adapter::AgentAdapter;
use llmenv::adapter::claude_code::ClaudeCodeAdapter;
use llmenv::config::Icm;
use llmenv::merge::{BundleRef, merge};
use tempfile::tempdir;

fn fixture_bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
    }
}

fn read_mcp_json(out: &std::path::Path) -> serde_json::Value {
    let s = std::fs::read_to_string(out.join("mcp.json")).expect("read mcp.json");
    serde_json::from_str(&s).expect("parse mcp.json")
}

fn icm_fixture() -> Icm {
    Icm {
        server_tag: "icm-server".into(),
        server_bind: "127.0.0.1:8765".into(),
        client_url: "http://icm.lan:8765".into(),
        default_topics: vec!["preferences".into()],
    }
}

#[test]
fn mcp_json_emitted_when_icm_present() {
    let bundles = vec![fixture_bundle("base")];
    let mut m = merge(&bundles).expect("merge");
    m.icm = Some(icm_fixture());
    m.icm_is_server = false;
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    assert!(
        tmp.path().join("mcp.json").exists(),
        "mcp.json should be emitted when manifest has Icm"
    );
}

#[test]
fn mcp_json_registers_http_client_when_not_server() {
    let bundles = vec![fixture_bundle("base")];
    let mut m = merge(&bundles).expect("merge");
    m.icm = Some(icm_fixture());
    m.icm_is_server = false;
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_mcp_json(tmp.path());
    let servers = v.get("mcpServers").expect("mcpServers key");
    let icm = servers.get("icm").expect("icm entry");
    assert_eq!(
        icm.get("url").and_then(|x| x.as_str()),
        Some("http://icm.lan:8765"),
        "client mode should register HTTP url"
    );
    assert!(
        icm.get("command").is_none(),
        "client mode should not have command field"
    );
}

#[test]
fn mcp_json_registers_stdio_server_when_is_server() {
    let bundles = vec![fixture_bundle("base")];
    let mut m = merge(&bundles).expect("merge");
    m.icm = Some(icm_fixture());
    m.icm_is_server = true;
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_mcp_json(tmp.path());
    let servers = v.get("mcpServers").expect("mcpServers key");
    let icm = servers.get("icm").expect("icm entry");
    assert!(
        icm.get("command").is_some(),
        "server mode should register stdio command"
    );
    assert!(
        icm.get("url").is_none(),
        "server mode should not have url field"
    );
}

#[test]
fn hook_template_substitutes_icm_mcp_placeholder() {
    let bundles = vec![fixture_bundle("with-icm-hook")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let hook =
        std::fs::read_to_string(tmp.path().join("hooks/pre-commit.json")).expect("read hook file");
    assert!(
        hook.contains("mcp://icm/recall"),
        "{{{{ICM_MCP}}}} should be substituted with the MCP server name; got: {hook}"
    );
    assert!(
        !hook.contains("{{ICM_MCP}}"),
        "no template placeholders should remain after materialization"
    );
}

#[test]
fn hook_without_template_is_passed_through_unchanged() {
    let bundles = vec![fixture_bundle("with-icm-hook")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let src =
        std::fs::read_to_string("tests/fixtures/bundles/with-icm-hook/hooks/no-template.json")
            .expect("read src");
    let dst = std::fs::read_to_string(tmp.path().join("hooks/no-template.json")).expect("read dst");
    assert_eq!(
        src, dst,
        "hook files without templates should be copied byte-for-byte"
    );
}

#[test]
fn mcp_json_absent_when_no_icm_config() {
    let bundles = vec![fixture_bundle("base")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    assert!(
        !tmp.path().join("mcp.json").exists(),
        "mcp.json should not be emitted when manifest has no Icm config"
    );
}
