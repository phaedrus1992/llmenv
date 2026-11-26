//! The session id that hook processes share through a file (#2909).

use super::*;

fn reply(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "content": [{ "type": "text", "text": text }] }
    }))
}

async fn count(server: &MockServer, method: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("requests")
        .iter()
        .filter(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body)
                .ok()
                .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
                .as_deref()
                == Some(method)
        })
        .count()
}

fn client_for(server: &MockServer, cache: &SessionCache) -> McpHttpClient {
    McpHttpClient::test_new(server.uri(), Duration::from_secs(2))
        .expect("valid URL")
        .with_session_cache(Some(cache.clone()))
}

/// Three processes make four calls and the server sees one `initialize`.
#[tokio::test]
async fn processes_that_share_a_cache_negotiate_one_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("initialize"))
        .respond_with(init_ok("shared-1", None))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("tools/call"))
        .and(wiremock::matchers::header("mcp-session-id", "shared-1"))
        .respond_with(reply("ok"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = SessionCache::at(dir.path().join("s.json"));
    for _ in 0..3 {
        let process = client_for(&server, &cache);
        process
            .call_tool("icm_memory_recall", serde_json::json!({}))
            .await
            .expect("call");
    }
    let process = client_for(&server, &cache);
    process
        .call_tool("icm_memory_recall", serde_json::json!({}))
        .await
        .expect("call");
    process
        .call_tool("icm_memory_recall", serde_json::json!({}))
        .await
        .expect("call");
    assert_eq!(count(&server, "initialize").await, 1);
}

/// The server rejects the saved id. The call retries with a new session, and the file holds the
/// new id for the next process.
#[tokio::test]
async fn a_rejected_saved_id_is_replaced_for_the_next_process() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("initialize"))
        .respond_with(init_ok("fresh", None))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("tools/call"))
        .and(wiremock::matchers::header("mcp-session-id", "dead"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("tools/call"))
        .and(wiremock::matchers::header("mcp-session-id", "fresh"))
        .respond_with(reply("recovered"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = SessionCache::at(dir.path().join("s.json"));
    cache.save("dead");
    let text = client_for(&server, &cache)
        .call_tool("icm_memory_recall", serde_json::json!({}))
        .await
        .expect("the retry must recover");
    assert_eq!(text, "recovered");
    assert_eq!(cache.load().as_deref(), Some("fresh"));
    assert_eq!(count(&server, "initialize").await, 1);
}

/// A probe must ask the server, so it ignores and replaces the saved id.
#[tokio::test]
async fn a_probe_ignores_the_saved_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("initialize"))
        .respond_with(init_ok("probed", None))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = SessionCache::at(dir.path().join("s.json"));
    cache.save("old");
    client_for(&server, &cache).probe().await.expect("probe");
    assert_eq!(count(&server, "initialize").await, 1);
    assert_eq!(cache.load().as_deref(), Some("probed"));
}

/// The `initialize` reply body is the only source of the server info, so that call is never
/// served from the file.
#[tokio::test]
async fn the_server_info_call_negotiates_even_with_a_saved_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(JsonRpcMethod("initialize"))
        .respond_with(init_ok("info", Some("use the tools")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = SessionCache::at(dir.path().join("s.json"));
    cache.save("old");
    let info = client_for(&server, &cache)
        .initialize_info()
        .await
        .expect("info");
    assert_eq!(info.instructions.as_deref(), Some("use the tools"));
    assert_eq!(count(&server, "initialize").await, 1);
}
