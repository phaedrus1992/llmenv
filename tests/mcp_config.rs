//! Tests for #58: the Claude Code adapter renders all resolved MCP servers
//! into `mcp.json`. (Generalizes #14's ICM-only server/client emission.)

use std::collections::BTreeMap;
use std::path::PathBuf;

use llmenv::adapter::AgentAdapter;
use llmenv::adapter::claude_code::ClaudeCodeAdapter;
use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};
use llmenv::merge::{BundleRef, merge};
use tempfile::tempdir;

fn fixture_bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
        precedence: 1,
    }
}

fn read_mcp_json(out: &std::path::Path) -> serde_json::Value {
    let s = std::fs::read_to_string(out.join("mcp.json")).expect("read mcp.json");
    serde_json::from_str(&s).expect("parse mcp.json")
}

fn stdio(name: &str, command: &str, args: &[&str]) -> ResolvedMcp {
    ResolvedMcp {
        name: name.into(),
        kind: ResolvedKind::Stdio {
            command: command.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            env: BTreeMap::new(),
        },
    }
}

fn remote(name: &str, url: &str) -> ResolvedMcp {
    ResolvedMcp {
        name: name.into(),
        kind: ResolvedKind::Remote {
            url: url.into(),
            transport: llmenv::config::McpTransport::Http,
        },
    }
}

#[test]
fn mcp_json_emitted_when_mcps_present() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.mcps = vec![remote("icm", "http://icm.lan:8765")];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    assert!(
        tmp.path().join("mcp.json").exists(),
        "mcp.json should be emitted when manifest has resolved MCPs"
    );
}

#[test]
fn mcp_json_registers_remote_client_url() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.mcps = vec![remote("icm", "http://icm.lan:8765")];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_mcp_json(tmp.path());
    let icm = v["mcpServers"]["icm"].clone();
    assert_eq!(
        icm.get("url").and_then(|x| x.as_str()),
        Some("http://icm.lan:8765")
    );
    assert!(icm.get("command").is_none(), "client mode has no command");
}

#[test]
fn mcp_json_registers_stdio_command() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.mcps = vec![stdio("icm", "icm", &["mcp-server"])];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_mcp_json(tmp.path());
    let icm = v["mcpServers"]["icm"].clone();
    assert_eq!(icm.get("command").and_then(|x| x.as_str()), Some("icm"));
    assert!(icm.get("url").is_none(), "server mode has no url");
}

#[test]
fn mcp_json_renders_multiple_servers() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.mcps = vec![
        stdio("playwright", "npx", &["-y", "@playwright/mcp@latest"]),
        remote("icm", "http://icm.lan:8765"),
    ];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_mcp_json(tmp.path());
    let servers = v["mcpServers"].as_object().expect("mcpServers object");
    assert_eq!(servers.len(), 2, "both servers should be registered");
    assert!(servers.contains_key("playwright"));
    assert!(servers.contains_key("icm"));
}

#[test]
fn hook_template_substitutes_icm_mcp_placeholder() {
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("with-icm-hook")],
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("with-icm-hook")],
    )
    .expect("merge");
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
fn mcp_json_absent_when_no_mcps() {
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    assert!(
        !tmp.path().join("mcp.json").exists(),
        "mcp.json should not be emitted when manifest has no resolved MCPs"
    );
}
