//! Minimal HTTP JSON-RPC MCP client — only the `tools/call` path this feature
//! needs. Not a general MCP library.

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};

mod body;
mod parse;
mod request_headers;

mod ssrf;

pub(crate) use parse::parse_tools_page;
use parse::{extract_text, parse_initialize_info, parse_rpc_body};
pub(crate) use ssrf::{SsrfPolicy, resolve_with_timeout, validate_url_production};
use ssrf::{redact_url, require_https_for_public};

/// A minimal MCP-over-HTTP client bound to one server URL with a fixed timeout.
#[derive(Debug, Clone)]
pub struct McpHttpClient {
    url: String,
    client: reqwest::Client,
    /// MCP's Streamable HTTP transport is session-scoped: the server hands out
    /// an `Mcp-Session-Id` on `initialize` and rejects `tools/call` without it
    /// ("Bad Request: Missing session ID"). Negotiated lazily on first call and
    /// cached so repeat calls on the same client reuse it. `tokio::sync::Mutex`
    /// (not `std::sync::Mutex`) because it's held across the `.await` in
    /// `ensure_session` — the workspace denies `await_holding_lock` for the
    /// std variant.
    session_id: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    /// What the server said in its last `initialize` reply (#2148).
    init_info: std::sync::Arc<tokio::sync::Mutex<Option<InitializeInfo>>>,
}

/// What an MCP server reports about itself in its `initialize` reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InitializeInfo {
    pub instructions: Option<String>,
}

/// One tool of a server's `tools/list` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolSummary {
    pub name: String,
    pub description: Option<String>,
}

/// The most `tools/list` pages [`McpHttpClient::list_tools`] follows, so a server that always
/// returns a cursor cannot loop doctor.
pub(crate) const MAX_TOOL_PAGES: usize = 50;

/// Internal result of one [`McpHttpClient::try_call_tool`] attempt: whether a
/// failure is worth clearing the cached session and retrying once (#1094), or
/// is fatal on the first try.
enum CallToolError {
    /// The server rejected the cached `Mcp-Session-Id` (HTTP 400/404) —
    /// clearing it and re-initializing may still succeed.
    StaleSession(anyhow::Error),
    /// Any other failure — retrying with a fresh session wouldn't help.
    Fatal(anyhow::Error),
}

impl From<CallToolError> for anyhow::Error {
    fn from(e: CallToolError) -> Self {
        match e {
            CallToolError::StaleSession(e) | CallToolError::Fatal(e) => e,
        }
    }
}

/// Whether `status` is the MCP Streamable HTTP transport's shape for "this
/// session id isn't valid" (mcp-proxy itself uses 400 for "Bad Request:
/// Missing session ID"; a dead/expired session is 404). Over-broad by
/// design: a 400 for an unrelated reason (e.g. malformed tool arguments)
/// also takes the stale-session retry path, wasting one re-initialize
/// before the real error surfaces on the second attempt — but it always
/// does surface, just one round trip later.
fn is_stale_session_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::NOT_FOUND
    )
}

/// Emit `[LLMENV_MCP_CALL] <tool> <duration>us` to stderr when
/// `LLMENV_TRACE_TIMING` is set (#1259) — the same env var that already gates
/// hook-run's per-phase timing markers and the cache hit/miss telemetry, so
/// there's one flag to know about, not three. Bare `eprintln!`, matching
/// `emit_trace_timing`'s precedent, for the same reason: a machine-parseable
/// protocol line must not depend on the tracing subscriber's config.
///
/// Deliberately reports timing only, not a result/entry count (the issue's
/// own examples show a count suffix only for `icm_memory_recall`, none for
/// `icm_memory_store`) — this client is a generic MCP transport with no
/// notion of what a "result" means for any given tool, and parsing ICM's
/// recall-specific text format here would be a layering violation.
fn emit_mcp_call_trace(tool: &str, elapsed: Duration, req_bytes: u64, resp_bytes: u64) {
    crate::mcp_trace::emit_mcp_call_trace(tool, elapsed, req_bytes, resp_bytes);
}

impl McpHttpClient {
    /// Build a client for `url` whose every request is bounded by `timeout`.
    ///
    /// # Errors
    /// Returns an error if the URL is invalid, uses an unsupported scheme, or
    /// points to a private/loopback IP address (SSRF protection).
    pub fn new(url: String, timeout: Duration) -> anyhow::Result<Self> {
        Self::with_headers(url, timeout, &std::collections::BTreeMap::new())
    }

    /// [`Self::new`] that also sends `headers` on every request, so a `Remote` entry's
    /// configured headers reach the probe (#2148).
    ///
    /// # Errors
    /// As [`Self::new`], and when a header name or value is not valid HTTP.
    pub(crate) fn with_headers(
        url: String,
        timeout: Duration,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<Self> {
        // Resolve and SSRF-validate up front, then pin reqwest to exactly the vetted
        // addresses. Pinning closes the DNS-rebinding TOCTOU: reqwest never re-resolves at
        // send() time, so the connection can only target an address we already approved.
        let (host, addrs) =
            validate_url_production(&url, SsrfPolicy::AllowPrivateNetwork, timeout)?;
        require_https_for_public(&url, &addrs)?;
        let map = request_headers::build(headers)?;
        // A redirect could reach a host that the address pin above does not cover, such as a
        // cloud metadata address, and could carry configured headers with it. An MCP endpoint
        // has no reason to redirect, so no redirect is followed.
        // A proxy resolves the host itself, so it would bypass the address pin above (#2484).
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(map)
            .no_proxy()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build HTTP client (TLS backend unavailable)")?;
        Ok(Self {
            url,
            client,
            session_id: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            init_info: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// The server URL without credentials or a query string, for error text that reaches a
    /// terminal.
    fn display_url(&self) -> String {
        redact_url(&self.url)
    }

    /// Negotiate an MCP session if one hasn't already been established on this
    /// client: send `initialize`, capture the `Mcp-Session-Id` response header
    /// (if the server sends one — plain non-session-scoped servers won't), and
    /// best-effort acknowledge with `notifications/initialized`.
    ///
    /// # Errors
    /// Network failure or a non-2xx status on the `initialize` request itself.
    async fn ensure_session(&self) -> anyhow::Result<Option<String>> {
        self.ensure_session_with(false).await
    }

    /// [`Self::ensure_session`]. With `capture_info`, the `initialize` reply body is read into
    /// `init_info`; the other callers never read it.
    async fn ensure_session_with(&self, capture_info: bool) -> anyhow::Result<Option<String>> {
        let mut cached = self.session_id.lock().await;
        if let Some(sid) = cached.as_ref() {
            return Ok(Some(sid.clone()));
        }

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

        *cached = sid.clone();
        Ok(sid)
    }

    /// Check that the server answers a fresh MCP `initialize` within the client timeout.
    ///
    /// # Errors
    /// Network failure, timeout, or a non-2xx status.
    pub async fn probe(&self) -> anyhow::Result<()> {
        // Drop any cached session: a probe must ask the server, not trust an old id.
        *self.session_id.lock().await = None;
        self.ensure_session().await.map(|_| ())
    }

    /// Ask the server for a fresh `initialize` and return what it said about itself.
    ///
    /// # Errors
    /// Network failure, timeout, a non-2xx status, or a reply with no readable `result`.
    pub(crate) async fn initialize_info(&self) -> anyhow::Result<InitializeInfo> {
        // Drop any cached session: the info must come from a fresh `initialize`.
        *self.session_id.lock().await = None;
        self.ensure_session_with(true).await?;
        Ok(self.init_info.lock().await.clone().unwrap_or_default())
    }

    /// List every tool of the server, following `nextCursor` for at most [`MAX_TOOL_PAGES`] pages.
    /// A session the server no longer knows is re-initialized once, as in [`Self::call_tool`].
    ///
    /// # Errors
    /// Network failure, a non-2xx status, a JSON-RPC `error`, a reply with no `result.tools`
    /// array, or more than [`MAX_TOOL_PAGES`] pages.
    pub(crate) async fn list_tools(&self) -> anyhow::Result<Vec<ToolSummary>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let body = match self.post_rpc("tools/list", &params, "tools/list").await {
                Ok(body) => body,
                Err(CallToolError::StaleSession(first_err)) => {
                    *self.session_id.lock().await = None;
                    self.post_rpc("tools/list", &params, "tools/list")
                        .await
                        .map_err(anyhow::Error::from)
                        .with_context(|| {
                            format!("retry after stale session; first attempt: {first_err}")
                        })?
                }
                Err(CallToolError::Fatal(e)) => return Err(e),
            };
            let (page, next) = parse_tools_page(body.get("result").unwrap_or(&Value::Null))?;
            tools.extend(page);
            cursor = next;
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(anyhow!(
            "tools/list returned more than {MAX_TOOL_PAGES} pages; stopped at the cap"
        ))
    }

    /// Call one MCP tool and return the concatenated text content.
    ///
    /// A cached `Mcp-Session-Id` the server no longer recognizes (expired, or
    /// the server restarted) comes back as HTTP 400 or 404 per the MCP
    /// Streamable HTTP transport (#1094: mcp-proxy itself uses 400 for
    /// "Bad Request: Missing session ID"). Rather than replaying that dead id
    /// forever — the only recovery previously being a fresh `llmenv` process —
    /// this clears the cache and re-initializes exactly once before giving up.
    ///
    /// # Errors
    /// Network failure, session negotiation failure, timeout, non-2xx status
    /// (after the one stale-session retry), a JSON-RPC `error` field, or a
    /// response missing `result.content[].text`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<String> {
        let start = std::time::Instant::now();
        crate::mcp_trace::count_mcp_call();
        let result = match self.try_call_tool(name, &arguments).await {
            Ok(text) => Ok(text),
            Err(CallToolError::StaleSession(first_err)) => {
                // Expires with the MCP server's session, so it is normal and debug only.
                tracing::debug!(
                    error = %first_err,
                    tool = %name,
                    "cached MCP session rejected, re-initializing and retrying once"
                );
                *self.session_id.lock().await = None;
                self.try_call_tool(name, &arguments)
                    .await
                    .map_err(anyhow::Error::from)
                    .with_context(|| {
                        format!("retry after stale session; first attempt: {first_err}")
                    })
            }
            Err(CallToolError::Fatal(e)) => Err(e),
        };
        // Only the single entry point for every MCP tool call this process
        // makes (#1259) — includes a stale-session retry's full cost when one
        // happens, which is exactly the "MCP proxy latency" #1259 wants
        // visible. Emitted on success only, matching emit_trace_timing's and
        // emit_cache_trace's precedent (the failure itself already logs via
        // the tracing::warn!/Action::run's own error path).
        if let Ok(text) = &result {
            let params = json!({ "name": name, "arguments": arguments });
            let req_bytes = params.to_string().len() as u64;
            let resp_bytes = text.len() as u64;
            emit_mcp_call_trace(name, start.elapsed(), req_bytes, resp_bytes);
        }
        result
    }

    /// One attempt at `call_tool`, distinguishing a stale-session rejection
    /// (worth clearing the cache and retrying once) from every other failure.
    async fn try_call_tool(&self, name: &str, arguments: &Value) -> Result<String, CallToolError> {
        let params = json!({ "name": name, "arguments": arguments });
        let body = self
            .post_rpc("tools/call", &params, &format!("tool {name}"))
            .await?;
        let text = extract_text(&body).ok_or_else(|| {
            CallToolError::Fatal(anyhow!(
                "tool {name} response missing result.content[].text"
            ))
        })?;
        // MCP reports a tool-level failure as a successful reply with `isError: true`. Treating
        // it as success would mark a failed store as stored in the idempotency set.
        if body["result"]["isError"].as_bool() == Some(true) {
            return Err(CallToolError::Fatal(anyhow!(
                "tool {name} reported an error: {text}"
            )));
        }
        Ok(text)
    }

    /// Send one JSON-RPC request on the session and return the decoded reply. `label` names the
    /// request in error text. The one request path for `tools/call` and `tools/list`.
    async fn post_rpc(
        &self,
        method: &str,
        params: &Value,
        label: &str,
    ) -> Result<Value, CallToolError> {
        let session_id = self.ensure_session().await.map_err(CallToolError::Fatal)?;

        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut builder = self
            .client
            .post(&self.url)
            // MCP's Streamable HTTP transport requires the client to accept
            // both response shapes a server may reply with. Without this,
            // mcp-proxy (fronting icm serve) 406s every call, which the SSRF
            // loopback block used to mask entirely for same-host setups.
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );
        if let Some(sid) = &session_id {
            builder = builder.header("mcp-session-id", sid);
        }
        let resp = builder
            .json(&req)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("POST {} for {label}", self.display_url()))
            .map_err(crate::proxy_note::with_proxy_note)
            .map_err(CallToolError::Fatal)?;

        // Capture status and body for detailed error reporting.
        let status = resp.status();
        if !status.is_success() {
            let body = body::error_body(resp, label).await;
            let err = anyhow!("{label} returned HTTP {}: {}", status, body);
            return Err(if is_stale_session_status(status) {
                CallToolError::StaleSession(err)
            } else {
                CallToolError::Fatal(err)
            });
        }

        let text = body::read_capped(resp, body::MAX_REPLY_BYTES, label)
            .await
            .map_err(CallToolError::Fatal)?;
        let body = parse_rpc_body(&text)
            .ok_or_else(|| anyhow!("decoding JSON response for {label}: the reply is not JSON"))
            .map_err(CallToolError::Fatal)?;

        if let Some(err) = body.get("error") {
            return Err(CallToolError::Fatal(anyhow!(
                "{label} JSON-RPC error: {err}"
            )));
        }
        Ok(body)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
