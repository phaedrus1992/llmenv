//! Minimal HTTP JSON-RPC MCP client — only the `tools/call` path this feature
//! needs. Not a general MCP library.

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};

/// A minimal MCP-over-HTTP client bound to one server URL with a fixed timeout.
#[derive(Debug, Clone)]
pub struct McpHttpClient {
    url: String,
    client: reqwest::Client,
}

impl McpHttpClient {
    /// Build a client for `url` whose every request is bounded by `timeout`.
    pub fn new(url: String, timeout: Duration) -> Self {
        // `build()` only fails on TLS backend init; default rustls is infallible
        // here, so fall back to a default client rather than panicking.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self { url, client }
    }

    /// Call one MCP tool and return the concatenated text content.
    ///
    /// # Errors
    /// Network failure, timeout, non-2xx status, a JSON-RPC `error` field, or a
    /// response missing `result.content[].text`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<String> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        let resp = self
            .client
            .post(&self.url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST {} for tool {name}", self.url))?
            .error_for_status()
            .with_context(|| format!("tool {name} returned an error status"))?;
        let body: Value = resp
            .json()
            .await
            .with_context(|| format!("decoding JSON response for tool {name}"))?;

        if let Some(err) = body.get("error") {
            return Err(anyhow!("tool {name} JSON-RPC error: {err}"));
        }
        extract_text(&body)
            .ok_or_else(|| anyhow!("tool {name} response missing result.content[].text"))
    }
}

/// Pull and concatenate every `text` entry from `result.content[]`.
fn extract_text(body: &Value) -> Option<String> {
    let content = body.get("result")?.get("content")?.as_array()?;
    let mut out = String::new();
    for item in content {
        if let Some(t) = item.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn call_tool_returns_text_content() {
        let server = MockServer::start().await;
        // MCP tools/call response: result.content[0].text
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "wake-up pack" }]
            }
        });
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2));
        let text = client
            .call_tool("icm_wake_up", serde_json::json!({}))
            .await
            .expect("call_tool ok");
        assert_eq!(text, "wake-up pack");
    }

    #[tokio::test]
    async fn call_tool_errors_on_unreachable() {
        // Port 0 is never listening; connection fails fast.
        let client =
            McpHttpClient::new("http://127.0.0.1:0".to_string(), Duration::from_millis(200));
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_tool_errors_on_jsonrpc_error() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32000, "message": "boom" }
        });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2));
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
