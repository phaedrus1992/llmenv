//! The MCP `initialize` handshake.

use anyhow::{Context, anyhow};
use serde_json::json;

use super::parse::parse_initialize_info;
use super::{McpHttpClient, body};

impl McpHttpClient {
    /// Send `initialize`, capture the `Mcp-Session-Id` response header (if the server sends
    /// one: plain non-session-scoped servers do not), and best-effort acknowledge with
    /// `notifications/initialized`. With `capture_info`, the `initialize` reply body is read
    /// into `init_info`.
    ///
    /// # Errors
    /// Network failure or a non-2xx status on the `initialize` request itself.
    pub(super) async fn negotiate_session(
        &self,
        capture_info: bool,
    ) -> anyhow::Result<Option<String>> {
        // #3017: time the `initialize` request and the `notifications/initialized` ack.
        let init_start = std::time::Instant::now();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "llmenv", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let resp = self
            .client
            .post(&self.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&init_req)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("POST {} for MCP initialize", self.display_url()))
            .map_err(crate::proxy_note::with_proxy_note)?;
        let init_req_bytes = init_req.to_string().len() as u64;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = body::error_body(resp, "MCP initialize").await;
            let resp_bytes = body.len() as u64;
            crate::mcp_trace::emit_mcp_handshake_trace(
                "initialize",
                init_start.elapsed(),
                Some(status.as_u16()),
                init_req_bytes,
                resp_bytes,
            );
            return Err(anyhow!("MCP initialize returned HTTP {status}: {body}"));
        }
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let init_resp_bytes = if capture_info {
            let text = body::read_capped(resp, body::MAX_REPLY_BYTES, "MCP initialize")
                .await
                .with_context(|| format!("reading the reply from {}", self.display_url()))?;
            *self.init_info.lock().await = Some(parse_initialize_info(&text)?);
            text.len() as u64
        } else {
            0
        };
        crate::mcp_trace::emit_mcp_handshake_trace(
            "initialize",
            init_start.elapsed(),
            None,
            init_req_bytes,
            init_resp_bytes,
        );

        if let Some(sid) = &sid {
            // Best-effort: some servers require this ack before accepting
            // further calls, others ignore it. Its own failure isn't fatal —
            // the subsequent tools/call attempt is the real signal, and it carries the cause.
            let ack_start = std::time::Instant::now();
            let ack_req = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
            let ack_req_bytes = ack_req.to_string().len() as u64;
            let ack = self
                .client
                .post(&self.url)
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .header("mcp-session-id", sid)
                .json(&ack_req)
                .send()
                .await;
            let ack_resp_bytes = if let Ok(r) = &ack {
                r.content_length().unwrap_or(0)
            } else {
                0
            };
            crate::mcp_trace::emit_mcp_handshake_trace(
                "notifications/initialized",
                ack_start.elapsed(),
                None,
                ack_req_bytes,
                ack_resp_bytes,
            );
            if let Err(e) = ack {
                tracing::debug!(
                    "MCP initialized notification to {} not sent: {e}",
                    self.display_url()
                );
            }
        }
        Ok(sid)
    }
}
