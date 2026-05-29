//! Minimal HTTP JSON-RPC MCP client — only the `tools/call` path this feature
//! needs. Not a general MCP library.

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use url::{Host, Url};

/// A minimal MCP-over-HTTP client bound to one server URL with a fixed timeout.
#[derive(Debug, Clone)]
pub struct McpHttpClient {
    url: String,
    client: reqwest::Client,
}

impl McpHttpClient {
    /// Build a client for `url` whose every request is bounded by `timeout`.
    ///
    /// # Errors
    /// Returns an error if the URL is invalid, uses an unsupported scheme, or
    /// points to a private/loopback IP address (SSRF protection).
    pub fn new(url: String, timeout: Duration) -> anyhow::Result<Self> {
        validate_url_production(&url)?;
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client (TLS backend unavailable)")?;
        Ok(Self { url, client })
    }

    #[cfg(test)]
    /// Build a client for testing, skipping SSRF validation.
    fn test_new(url: String, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { url, client })
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
            .with_context(|| format!("POST {} for tool {name}", self.url))?;

        // Capture status and body for detailed error reporting.
        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "(failed to read error body)".to_string());
            return Err(anyhow!("tool {name} returned HTTP {}: {}", status, body));
        }

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

/// Validate ICM backend URL to prevent SSRF attacks.
///
/// Rejects URLs with unsupported schemes and private/loopback IP addresses.
fn validate_url_production(url: &str) -> anyhow::Result<()> {
    let parsed = Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;

    // Only allow http/https schemes
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!(
            "unsupported URL scheme '{}' (only http/https allowed)",
            parsed.scheme()
        ));
    }

    // Reject private/loopback IP ranges to prevent SSRF
    if let Some(host) = parsed.host() {
        match host {
            Host::Ipv4(v4) => {
                if v4.is_loopback() {
                    return Err(anyhow!("loopback IPv4 {} not allowed", v4));
                }
                if v4.is_private() {
                    return Err(anyhow!("private IPv4 {} not allowed", v4));
                }
                if v4.is_link_local() {
                    return Err(anyhow!("link-local IPv4 {} not allowed", v4));
                }
            }
            Host::Ipv6(v6) => {
                if v6.is_loopback() {
                    return Err(anyhow!("loopback IPv6 {} not allowed", v6));
                }
                if v6.is_unicast_link_local() {
                    return Err(anyhow!("link-local IPv6 {} not allowed", v6));
                }
            }
            Host::Domain(_) => {} // Domain names are allowed
        }
    }

    Ok(())
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

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let text = client
            .call_tool("icm_wake_up", serde_json::json!({}))
            .await
            .expect("call_tool ok");
        assert_eq!(text, "wake-up pack");
    }

    #[tokio::test]
    async fn call_tool_errors_on_unreachable() {
        // Valid public IP that should reject (no listening service)
        let client = McpHttpClient::new("http://8.8.8.8:0".to_string(), Duration::from_millis(200))
            .expect("valid URL");
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn validate_url_rejects_loopback() {
        assert!(validate_url_production("http://127.0.0.1:8080").is_err());
        assert!(validate_url_production("http://[::1]:8080").is_err());
    }

    #[test]
    fn validate_url_rejects_private_ips() {
        assert!(validate_url_production("http://10.0.0.1:8080").is_err());
        assert!(validate_url_production("http://192.168.1.1:8080").is_err());
        assert!(validate_url_production("http://172.16.0.1:8080").is_err());
    }

    #[test]
    fn validate_url_accepts_public_ips() {
        assert!(validate_url_production("http://8.8.8.8:8080").is_ok());
        assert!(validate_url_production("https://example.com:8080").is_ok());
    }

    #[test]
    fn validate_url_rejects_unsupported_schemes() {
        assert!(validate_url_production("file:///tmp/socket").is_err());
        assert!(validate_url_production("ftp://example.com").is_err());
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
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn extract_text_handles_missing_result() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1
        });
        assert_eq!(extract_text(&body), None);
    }

    #[test]
    fn extract_text_handles_missing_content() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "other_field": "data" }
        });
        assert_eq!(extract_text(&body), None);
    }

    #[test]
    fn extract_text_handles_non_array_content() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": "not an array" }
        });
        assert_eq!(extract_text(&body), None);
    }

    #[test]
    fn extract_text_concatenates_multiple_text_items() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    { "type": "text", "text": "first" },
                    { "type": "text", "text": "second" },
                    { "type": "text", "text": "third" }
                ]
            }
        });
        assert_eq!(
            extract_text(&body),
            Some("first\nsecond\nthird".to_string())
        );
    }

    #[test]
    fn extract_text_skips_non_text_items() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    { "type": "text", "text": "a" },
                    { "type": "image", "url": "https://example.com/img.png" },
                    { "type": "text", "text": "b" }
                ]
            }
        });
        assert_eq!(extract_text(&body), Some("a\nb".to_string()));
    }

    #[test]
    fn extract_text_handles_missing_text_field() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    { "type": "text" },
                    { "type": "text", "text": "valid" }
                ]
            }
        });
        assert_eq!(extract_text(&body), Some("valid".to_string()));
    }

    #[test]
    fn extract_text_handles_empty_content_array() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": [] }
        });
        assert_eq!(extract_text(&body), Some(String::new()));
    }
}
