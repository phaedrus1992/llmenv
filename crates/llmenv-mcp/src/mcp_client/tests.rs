//! Test helpers shared by the MCP client tests.
use super::ssrf::{blocked_reason, drop_link_local_for_domain, is_link_local, run_with_timeout};
use super::*;
use crate::cleartext::{embedded_ipv4, is_cleartext_safe, is_unique_local_v6};
use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::thread;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// #1576/#1610: `MockServer::start()` binds a real ephemeral TCP port, same as `proxy.rs`'s own
// network tests. Sharing `port_guard()` closes the race with their bind-then-probe checks.
use llmenv_util::testkit::port_guard;

mod calls;
mod listing_and_headers;
mod probe_and_info;
mod proxy_note;
mod shared_session;
mod ssrf_addrs;
mod ssrf_url;
mod text_and_timeouts;

/// Matches a JSON-RPC request body by its `method` field, regardless of
/// other fields — lets a single mock path discriminate `initialize` from
/// `tools/call` requests the way a real MCP server does.
struct JsonRpcMethod(&'static str);

impl wiremock::Match for JsonRpcMethod {
    fn matches(&self, request: &wiremock::Request) -> bool {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
            .as_deref()
            == Some(self.0)
    }
}

fn init_ok(sid: &str, instructions: Option<&str>) -> ResponseTemplate {
    let mut result = serde_json::json!({ "protocolVersion": "2025-06-18" });
    if let Some(text) = instructions {
        result["instructions"] = text.into();
    }
    ResponseTemplate::new(200)
        .insert_header("mcp-session-id", sid)
        .set_body_json(serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": result }))
}

/// Matches a `tools/list` request by its `params.cursor`.
struct Cursor(Option<&'static str>);

impl wiremock::Match for Cursor {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        body["params"]["cursor"].as_str() == self.0
    }
}

fn tools_page(names: &[&str], next: Option<&str>) -> ResponseTemplate {
    let tools: Vec<_> = names
        .iter()
        .map(|n| serde_json::json!({ "name": n, "description": format!("about {n}") }))
        .collect();
    let mut result = serde_json::json!({ "tools": tools });
    if let Some(cursor) = next {
        result["nextCursor"] = cursor.into();
    }
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
}

async fn server_with_session(instructions: Option<&str>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("initialize"))
        .respond_with(init_ok("s1", instructions))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    server
}

#[test]
fn a_recall_breaker_path_is_kept_until_it_is_replaced() -> anyhow::Result<()> {
    let client = McpHttpClient::test_new("http://127.0.0.1:9".to_string(), Duration::from_secs(1))?;
    assert_eq!(client.recall_breaker(), None);
    let client = client.with_recall_breaker(Some(std::path::PathBuf::from("breaker.json")));
    assert_eq!(
        client.recall_breaker(),
        Some(std::path::Path::new("breaker.json"))
    );
    Ok(())
}
