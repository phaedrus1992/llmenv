//! Minimal HTTP JSON-RPC MCP client — only the `tools/call` path this feature
//! needs. Not a general MCP library.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use url::{Host, Url};

use crate::mcp::cleartext::{embedded_ipv4, is_cleartext_safe, is_unique_local_v6};

/// Which address ranges [`validate_url_production`] allows past the SSRF gate.
///
/// The gate is shared by two callers with different trust models: llmenv's own
/// ICM memory/transcript backend (`icm serve`) is *expected* to live on
/// loopback or the operator's LAN (AGENTS.md: "the resolved icm MCP endpoint
/// can be a remote `icm serve`"), while a configured third-party SaaS API
/// (e.g. the `umans` throttle backend) should never legitimately resolve
/// inside a private network — that would itself be a sign of a misconfigured
/// or hijacked endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SsrfPolicy {
    /// Reject loopback, private (RFC 1918 / ULA), link-local, unspecified, and
    /// broadcast addresses. For endpoints expected to be public internet
    /// services.
    PublicOnly,
    /// Reject only link-local, unspecified, and broadcast addresses (still
    /// blocks classic SSRF/metadata targets with no legitimate use here).
    /// Loopback and private ranges are allowed — the expected topology for
    /// llmenv's own same-host or LAN-hosted ICM backend.
    AllowPrivateNetwork,
}

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
fn emit_mcp_call_trace(tool: &str, elapsed: Duration) {
    if std::env::var_os("LLMENV_TRACE_TIMING").is_none() {
        return;
    }
    let us = elapsed.as_micros();
    eprintln!("[LLMENV_MCP_CALL] {tool} {us}us");
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
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("MCP header name '{name}' is not valid"))?;
            let mut value = reqwest::header::HeaderValue::from_str(value)
                .with_context(|| format!("MCP header '{name}' has a value that is not valid"))?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        // A redirect could reach a host that the address pin above does not cover, such as a
        // cloud metadata address, and could carry configured headers with it. An MCP endpoint
        // has no reason to redirect, so no redirect is followed.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(map)
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

    #[cfg(test)]
    /// Build a client for testing, skipping SSRF validation.
    pub(crate) fn test_new(url: String, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client")?;
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
            .with_context(|| format!("POST {} for MCP initialize", self.display_url()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp
                .text()
                .await
                .inspect_err(|e| {
                    tracing::warn!(
                        error = %e,
                        url = %self.url,
                        "failed to read MCP initialize error response body"
                    )
                })
                .unwrap_or_else(|_| "(failed to read error body)".to_string());
            return Err(anyhow!("MCP initialize returned HTTP {status}: {body}"));
        }
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if capture_info {
            let text = resp.text().await.with_context(|| {
                format!(
                    "reading the MCP initialize reply from {}",
                    self.display_url()
                )
            })?;
            *self.init_info.lock().await = Some(parse_initialize_info(&text)?);
        }

        if let Some(sid) = &sid {
            // Best-effort: some servers require this ack before accepting
            // further calls, others ignore it. Its own failure isn't fatal —
            // the subsequent tools/call attempt is the real signal.
            let _ = self
                .client
                .post(&self.url)
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .header("mcp-session-id", sid)
                .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .send()
                .await;
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
        let result = match self.try_call_tool(name, &arguments).await {
            Ok(text) => Ok(text),
            Err(CallToolError::StaleSession(first_err)) => {
                tracing::warn!(
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
        if result.is_ok() {
            emit_mcp_call_trace(name, start.elapsed());
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
            .with_context(|| format!("POST {} for {label}", self.display_url()))
            .map_err(CallToolError::Fatal)?;

        // Capture status and body for detailed error reporting.
        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .inspect_err(|e| {
                    tracing::warn!(
                        error = %e,
                        request = %label,
                        "failed to read MCP error response body"
                    )
                })
                .unwrap_or_else(|_| "(failed to read error body)".to_string());
            let err = anyhow!("{label} returned HTTP {}: {}", status, body);
            return Err(if is_stale_session_status(status) {
                CallToolError::StaleSession(err)
            } else {
                CallToolError::Fatal(err)
            });
        }

        let text = resp
            .text()
            .await
            .with_context(|| format!("decoding JSON response for {label}"))
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

/// Decode a JSON-RPC reply body, which is plain JSON or an SSE stream. A plain body is returned
/// as it is. An SSE stream gives the first `data:` event that has a `result` or an `error`, so a
/// notification event ahead of the reply is skipped.
fn parse_rpc_body(body: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
        return Some(value);
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
}

/// Read `result.instructions` from an `initialize` reply.
fn parse_initialize_info(body: &str) -> anyhow::Result<InitializeInfo> {
    let reply = parse_rpc_body(body).context("the initialize reply is not JSON or SSE")?;
    if let Some(error) = reply.get("error") {
        return Err(anyhow!("MCP initialize JSON-RPC error: {error}"));
    }
    let result = reply
        .get("result")
        .context("the initialize reply has no result")?;
    let instructions = match result.get("instructions") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => return Err(anyhow!("initialize instructions is not a string: {other}")),
    };
    Ok(InitializeInfo { instructions })
}

/// Read one `tools/list` `result`: its tools and its `nextCursor`. Both the HTTP and the stdio
/// probe use it, so the two cannot disagree.
///
/// # Errors
/// The result has no `tools` array, or a tool has no string `name` or a `description` that is not
/// a string.
pub(crate) fn parse_tools_page(
    result: &Value,
) -> anyhow::Result<(Vec<ToolSummary>, Option<String>)> {
    let listed = result
        .get("tools")
        .and_then(Value::as_array)
        .context("tools/list reply has no result.tools array")?;
    let mut tools = Vec::with_capacity(listed.len());
    for (index, tool) in listed.iter().enumerate() {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .with_context(|| format!("tools/list entry {index} has no string name"))?;
        let description = match tool.get("description") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => {
                return Err(anyhow!(
                    "tool '{name}' has a description that is not a string"
                ));
            }
        };
        tools.push(ToolSummary {
            name: name.to_string(),
            description,
        });
    }
    let next = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(String::from);
    Ok((tools, next))
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

/// Report why an IP address is blocked for SSRF safety, or `None` if it is a
/// routable public address.
///
/// Single source of truth shared by literal-IP and resolved-domain validation.
/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are unwrapped and judged by
/// their IPv4 form so a blocked v4 range cannot be smuggled through the v6
/// namespace (#191).
fn blocked_reason(ip: &IpAddr, policy: SsrfPolicy) -> Option<&'static str> {
    let block_private = policy == SsrfPolicy::PublicOnly;
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                block_private.then_some("loopback IPv4")
            } else if v4.is_private() {
                block_private.then_some("private IPv4")
            } else if v4.is_link_local() {
                Some("link-local IPv4")
            } else if v4.is_unspecified() {
                Some("unspecified IPv4")
            } else if v4.is_broadcast() {
                Some("broadcast IPv4")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if let Some(embedded) = embedded_ipv4(v6) {
                return blocked_reason(&IpAddr::V4(embedded), policy);
            }
            if v6.is_loopback() {
                block_private.then_some("loopback IPv6")
            } else if v6.is_unspecified() {
                Some("unspecified IPv6")
            } else if v6.is_unicast_link_local() {
                Some("link-local IPv6")
            } else if is_unique_local_v6(v6) {
                block_private.then_some("unique-local IPv6 (ULA)")
            } else {
                None
            }
        }
    }
}

/// Resolve `(host, port)` to socket addresses, bounded by `timeout`.
///
/// `ToSocketAddrs::to_socket_addrs()` is a blocking syscall with no timeout of
/// its own. Under DNS resolver failure (unreachable nameserver, VPN drop,
/// flaky mDNS) it can hang for minutes — long enough to defeat every caller's
/// own request timeout, since resolution happens before any HTTP client
/// exists to enforce one. Bound it by racing it against `timeout` on a
/// dedicated thread; on timeout the thread is abandoned rather than joined —
/// it dies with the process (these are short-lived CLI invocations), and
/// there is no portable way to cancel a blocked `getaddrinfo()` call.
pub(crate) fn resolve_with_timeout(
    host: &str,
    port: u16,
    timeout: Duration,
) -> anyhow::Result<Vec<SocketAddr>> {
    let host_owned = host.to_string();
    let result = run_with_timeout(
        move || {
            (host_owned.as_str(), port)
                .to_socket_addrs()
                .map(|it| it.collect::<Vec<_>>())
        },
        timeout,
    );
    match result {
        Ok(Ok(addrs)) => Ok(addrs),
        Ok(Err(e)) => Err(anyhow!("failed to resolve host '{host}': {e}")),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
            "DNS resolution for host '{host}' timed out after {timeout:?}"
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow!("DNS resolution for host '{host}' thread panicked"))
        }
    }
}

/// Race an arbitrary blocking closure against `timeout` on a dedicated thread.
///
/// Split out from `resolve_with_timeout` so the timeout/race mechanism itself
/// can be tested deterministically (a controllable closure) instead of racing
/// against real DNS resolution — the latter timed out or not depending on
/// scheduling noise (thread spawn latency, resolver cache state), not on the
/// mechanism under test.
fn run_with_timeout<T, F>(f: F, timeout: Duration) -> Result<T, mpsc::RecvTimeoutError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout)
}

/// The URL without credentials, query string, or fragment, for error text that reaches a
/// terminal.
fn redact_url(url: &str) -> String {
    Url::parse(url).map_or_else(
        |_| "(invalid URL)".to_string(),
        |mut u| {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        },
    )
}

/// Refuse `http://` when any vetted address is outside the operator's own network. The
/// remote `icm serve` topology runs on loopback or the operator's LAN, so cleartext stays
/// allowed there (#2476).
fn require_https_for_public(url: &str, addrs: &[SocketAddr]) -> anyhow::Result<()> {
    let parsed = Url::parse(url).context("invalid URL")?;
    if parsed.scheme() != "http" {
        return Ok(());
    }
    match addrs.iter().find(|a| !is_cleartext_safe(&a.ip())) {
        Some(addr) => Err(anyhow!(
            "refusing cleartext http:// to {} ({}) for {}: use https://, or move the server \
             onto a loopback, private, or Tailscale address",
            addr.ip(),
            parsed.host_str().unwrap_or("?"),
            redact_url(url)
        )),
        None => Ok(()),
    }
}

/// Drop link-local addresses from a resolved domain under `AllowPrivateNetwork`.
///
/// mDNS names (`host.local`) resolve to a reachable address plus `fe80::` addresses for
/// every interface. The caller pins reqwest to the returned list, so a dropped address is
/// never connected to and the metadata-endpoint protection holds (#2512). `PublicOnly`
/// keeps every address, so the caller rejects on the first blocked one.
///
/// # Errors
/// The host resolved to no addresses, or only to link-local addresses.
fn drop_link_local_for_domain(
    resolved: Vec<SocketAddr>,
    policy: SsrfPolicy,
    url: &str,
) -> anyhow::Result<Vec<SocketAddr>> {
    if resolved.is_empty() {
        return Err(anyhow!(
            "host of URL {} resolved to no addresses",
            redact_url(url)
        ));
    }
    if policy != SsrfPolicy::AllowPrivateNetwork {
        return Ok(resolved);
    }
    let is_link_local = |addr: &SocketAddr| {
        matches!(
            blocked_reason(&addr.ip(), policy),
            Some("link-local IPv4" | "link-local IPv6")
        )
    };
    let kept: Vec<SocketAddr> = resolved
        .iter()
        .copied()
        .filter(|a| !is_link_local(a))
        .collect();
    match (kept.is_empty(), resolved.iter().find(|a| is_link_local(a))) {
        (true, Some(dropped)) => Err(anyhow!(
            "host of URL {} resolved only to link-local address {} (SSRF); \
             no routable address is left",
            redact_url(url),
            dropped.ip()
        )),
        _ => Ok(kept),
    }
}

/// Validate ICM backend URL to prevent SSRF attacks and return the host string
/// together with the vetted set of socket addresses the connection may target.
///
/// Rejects unsupported schemes, blocked literal IPs, and — for hostnames —
/// resolves DNS and rejects the URL if *any* resolved address is blocked, except that
/// `AllowPrivateNetwork` first drops link-local addresses of a domain host (#2512). The
/// caller pins reqwest to the returned host→addrs mapping so reqwest cannot
/// re-resolve to an unvetted address at send() time (DNS-rebinding TOCTOU
/// mitigation, #191). The host is returned from the same parse that produced the
/// addresses, so the caller never re-parses the URL — a second parse could
/// disagree about the host and pin the wrong (or no) mapping.
///
/// `policy` decides whether loopback/private ranges are allowed (see
/// [`SsrfPolicy`]); `timeout` bounds DNS resolution for domain hosts, in
/// addition to whatever the caller uses to bound the request itself.
///
/// # Errors
/// Returns an error for an unparseable URL, an unsupported scheme, a missing
/// host, a DNS resolution failure or timeout, or any resolved/literal address
/// blocked under the given `policy`.
pub(crate) fn validate_url_production(
    url: &str,
    policy: SsrfPolicy,
    timeout: Duration,
) -> anyhow::Result<(String, Vec<SocketAddr>)> {
    // The URL can carry credentials or a token in its query, so no error below prints it raw.
    let parsed = Url::parse(url).context("invalid URL")?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!(
            "unsupported URL scheme '{}' (only http/https allowed)",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host()
        .ok_or_else(|| anyhow!("URL {} has no host", redact_url(url)))?;
    // reqwest's resolve_to_addrs keys on the unbracketed host string; Host's
    // Display matches host_str without the IPv6 brackets, which is what we pin.
    let host_key = match host {
        Host::Ipv4(v4) => v4.to_string(),
        Host::Ipv6(v6) => v6.to_string(),
        Host::Domain(name) => name.to_string(),
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("URL {} has no port and an unknown default", redact_url(url)))?;

    let addrs: Vec<SocketAddr> = match host {
        Host::Ipv4(v4) => vec![SocketAddr::new(IpAddr::V4(v4), port)],
        Host::Ipv6(v6) => vec![SocketAddr::new(IpAddr::V6(v6), port)],
        Host::Domain(name) => {
            let resolved = resolve_with_timeout(name, port, timeout)?;
            drop_link_local_for_domain(resolved, policy, url)?
        }
    };

    // Reject if ANY remaining address is blocked. A permissive "some address is
    // public" rule would let an attacker pair one public A record with a private
    // one and gamble on connection ordering.
    for addr in &addrs {
        if let Some(reason) = blocked_reason(&addr.ip(), policy) {
            return Err(anyhow!("{reason} address {} not allowed (SSRF)", addr.ip()));
        }
    }

    Ok((host_key, addrs))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn emit_mcp_call_trace_never_panics_without_env_var() {
        emit_mcp_call_trace("icm_memory_recall", Duration::from_micros(2340));
    }

    #[tokio::test]
    async fn call_tool_returns_an_error_when_the_tool_reports_is_error() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "isError": true,
                "content": [{ "type": "text", "text": "database is locked" }]
            }
        });
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let err = client
            .call_tool("icm_memory_store", serde_json::json!({}))
            .await
            .expect_err("a tool error must not look like success")
            .to_string();
        assert!(err.contains("database is locked"), "{err}");
        assert!(err.contains("icm_memory_store"), "{err}");
    }

    #[tokio::test]
    async fn call_tool_treats_is_error_false_as_success() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "isError": false, "content": [{ "type": "text", "text": "ok" }] }
        });
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let text = client
            .call_tool("icm_memory_store", serde_json::json!({}))
            .await
            .expect("isError false is success");
        assert_eq!(text, "ok");
    }

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

    #[tokio::test]
    async fn probe_succeeds_when_server_answers_initialize() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("mcp-session-id", "s1")
                    .set_body_json(serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("notifications/initialized"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        client.probe().await.expect("probe ok");
    }

    #[tokio::test]
    async fn probe_errors_on_http_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let err = client.probe().await.expect_err("500 must fail the probe");
        assert!(err.to_string().contains("500"), "got: {err}");
    }

    #[tokio::test]
    async fn probe_errors_when_server_accepts_but_never_answers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_millis(200)).expect("valid URL");
        assert!(client.probe().await.is_err(), "a wedged server must fail");
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

    #[tokio::test]
    async fn initialize_info_returns_the_instructions() {
        let server = server_with_session(Some("be careful")).await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let info = client.initialize_info().await.expect("info");
        assert_eq!(info.instructions.as_deref(), Some("be careful"));
    }

    #[tokio::test]
    async fn initialize_info_is_empty_when_the_server_sends_no_instructions() {
        let server = server_with_session(None).await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        assert_eq!(
            client.initialize_info().await.expect("info"),
            InitializeInfo::default()
        );
    }

    #[test]
    fn initialize_info_reads_json_and_sse_bodies() {
        let json = r#"{"result":{"instructions":"a"}}"#;
        assert_eq!(
            parse_initialize_info(json).unwrap().instructions.as_deref(),
            Some("a")
        );
        let sse = format!("event: message\ndata: {json}\n\n");
        assert_eq!(
            parse_initialize_info(&sse).unwrap().instructions.as_deref(),
            Some("a")
        );
        // A notification event ahead of the reply is skipped.
        let two = format!("data: {{\"method\":\"notifications/x\"}}\n\ndata: {json}\n\n");
        assert_eq!(
            parse_initialize_info(&two).unwrap().instructions.as_deref(),
            Some("a")
        );
        let none = r#"{"result":{}}"#;
        assert_eq!(
            parse_initialize_info(none).unwrap(),
            InitializeInfo::default()
        );
    }

    #[test]
    fn initialize_info_fails_loudly_on_a_reply_it_cannot_read() {
        for body in [
            "not json",
            "",
            r#"{"id":0}"#,
            r#"{"result":{"instructions":5}}"#,
            r#"{"error":{"message":"no"}}"#,
        ] {
            assert!(parse_initialize_info(body).is_err(), "{body}");
        }
    }

    #[test]
    fn a_tools_page_rejects_malformed_entries() {
        let ok = serde_json::json!({"tools":[{"name":"a"},{"name":"b","description":null}],"nextCursor":"n"});
        let (tools, next) = parse_tools_page(&ok).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(next.as_deref(), Some("n"));
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"tools":[{"description":"x"}]}),
            serde_json::json!({"tools":[{"name":3}]}),
            serde_json::json!({"tools":[{"name":"a","description":4}]}),
        ] {
            assert!(parse_tools_page(&bad).is_err(), "{bad}");
        }
        let empty_cursor = serde_json::json!({"tools":[],"nextCursor":""});
        assert_eq!(parse_tools_page(&empty_cursor).unwrap().1, None);
    }

    #[test]
    fn error_text_hides_the_url_query_and_credentials() {
        let client = McpHttpClient::test_new(
            "http://user:pw@127.0.0.1:9/mcp?token=secret#f".into(),
            Duration::from_secs(1),
        )
        .unwrap();
        let shown = client.display_url();
        assert!(!shown.contains("secret") && !shown.contains("pw") && !shown.contains("user"));
        assert!(shown.contains("127.0.0.1:9/mcp"));
    }

    proptest::proptest! {
        #[test]
        fn an_initialize_reply_reads_the_same_as_json_or_sse(text in "[ -~é]{0,200}") {
            let json = serde_json::json!({"result": {"instructions": text}}).to_string();
            let sse = format!("event: message\ndata: {json}\n\n");
            let plain = parse_initialize_info(&json).unwrap();
            proptest::prop_assert_eq!(&plain, &parse_initialize_info(&sse).unwrap());
            proptest::prop_assert_eq!(plain.instructions, Some(text));
        }

        #[test]
        fn no_body_makes_the_reply_parser_panic(body in "\\PC{0,200}") {
            let _ = parse_initialize_info(&body);
            let _ = parse_rpc_body(&body);
        }
    }

    #[tokio::test]
    async fn initialize_info_fails_on_an_unreadable_reply_from_a_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("garbage"))
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        assert!(client.initialize_info().await.is_err());
    }

    #[tokio::test]
    async fn list_tools_reads_an_sse_reply() {
        let server = server_with_session(None).await;
        let payload = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"s"}]}});
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("event: message\ndata: {payload}\n\n")),
            )
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        assert_eq!(client.list_tools().await.unwrap()[0].name, "s");
    }

    #[tokio::test]
    async fn a_client_without_headers_does_not_follow_a_redirect() {
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(init_ok("s", None))
            .expect(0)
            .mount(&target)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", target.uri()))
            .mount(&server)
            .await;
        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2)).expect("client");
        assert!(client.probe().await.is_err());
    }

    #[tokio::test]
    async fn with_headers_does_not_follow_a_redirect() {
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(init_ok("s", None))
            .expect(0)
            .mount(&target)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", target.uri()))
            .mount(&server)
            .await;
        let headers =
            std::collections::BTreeMap::from([("x-api-key".to_string(), "k".to_string())]);
        let client = McpHttpClient::with_headers(server.uri(), Duration::from_secs(2), &headers)
            .expect("client");
        assert!(client.probe().await.is_err());
    }

    #[tokio::test]
    async fn list_tools_joins_the_pages_in_order() {
        let server = server_with_session(None).await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .and(Cursor(None))
            .respond_with(tools_page(&["a", "b"], Some("p2")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .and(Cursor(Some("p2")))
            .respond_with(tools_page(&["c"], None))
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let tools = client.list_tools().await.expect("tools");
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(tools[2].description.as_deref(), Some("about c"));
    }

    #[tokio::test]
    async fn list_tools_with_one_page_makes_one_request() {
        let server = server_with_session(None).await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .respond_with(tools_page(&["only"], None))
            .expect(1)
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        assert_eq!(client.list_tools().await.expect("tools").len(), 1);
    }

    #[tokio::test]
    async fn list_tools_stops_at_the_page_cap_and_names_it() {
        let server = server_with_session(None).await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .respond_with(tools_page(&["t"], Some("again")))
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let err = client.list_tools().await.expect_err("cap").to_string();
        assert!(err.contains(&MAX_TOOL_PAGES.to_string()), "{err}");
    }

    #[tokio::test]
    async fn list_tools_reinitializes_once_after_a_stale_session() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(init_ok("sess-stale", None))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(init_ok("sess-fresh", None))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .and(wiremock::matchers::header("mcp-session-id", "sess-stale"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .and(wiremock::matchers::header("mcp-session-id", "sess-fresh"))
            .respond_with(tools_page(&["ok"], None))
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        assert_eq!(client.list_tools().await.expect("tools")[0].name, "ok");
    }

    #[tokio::test]
    async fn list_tools_fails_on_a_reply_without_a_tools_array() {
        let server = server_with_session(None).await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
            )
            .mount(&server)
            .await;
        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let err = client.list_tools().await.expect_err("no tools").to_string();
        assert!(err.contains("result.tools"), "{err}");
    }

    #[tokio::test]
    async fn with_headers_sends_the_configured_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::header("x-api-key", "secret"))
            .respond_with(init_ok("s1", None))
            .expect(2) // the initialize and its acknowledgement both carry the header
            .mount(&server)
            .await;
        let headers =
            std::collections::BTreeMap::from([("x-api-key".to_string(), "secret".to_string())]);
        let client = McpHttpClient::with_headers(server.uri(), Duration::from_secs(2), &headers)
            .expect("client");
        client.probe().await.expect("probe");
    }

    #[test]
    fn with_headers_rejects_an_invalid_header_name_and_value() {
        for (name, value) in [("bad name", "v"), ("x-ok", "line\nbreak")] {
            let headers = std::collections::BTreeMap::from([(name.to_string(), value.to_string())]);
            let err = McpHttpClient::with_headers(
                "http://127.0.0.1:9".into(),
                Duration::from_secs(1),
                &headers,
            )
            .expect_err("invalid header")
            .to_string();
            assert!(err.contains("header"), "{err}");
        }
    }

    #[tokio::test]
    async fn probe_ignores_a_cached_session() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(ResponseTemplate::new(200).insert_header("mcp-session-id", "s1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        client.probe().await.expect("first probe ok");
        assert!(
            client.probe().await.is_err(),
            "second probe must re-ask the server"
        );
    }

    #[tokio::test]
    async fn call_tool_negotiates_session_and_sends_it_on_tools_call() {
        // Regression test: a real MCP Streamable HTTP server (mcp-proxy fronting
        // icm serve) hands out an Mcp-Session-Id on initialize and rejects
        // tools/call with "Bad Request: Missing session ID" without it. Before
        // the initialize handshake was added, every real call failed this way —
        // masked in practice by the (also buggy) SSRF loopback block, which
        // rejected the URL before any request was ever sent.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("mcp-session-id", "test-session-abc")
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 0, "result": { "protocolVersion": "2025-06-18" }
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(JsonRpcMethod("notifications/initialized"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(JsonRpcMethod("tools/call"))
            .and(wiremock::matchers::header(
                "mcp-session-id",
                "test-session-abc",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "content": [{ "type": "text", "text": "recalled" }] }
            })))
            .mount(&server)
            .await;
        // Without the "mcp-session-id" header requirement above, a tools/call
        // missing the header would 404 (no matcher) — the real server's 400
        // "Missing session ID" — surfacing the same failure mode.

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let text = client
            .call_tool("icm_memory_recall", serde_json::json!({}))
            .await
            .expect("call_tool ok — session must be negotiated automatically");
        assert_eq!(text, "recalled");
    }

    /// #1094: the server rejecting a cached `Mcp-Session-Id` (session expired,
    /// or the server restarted) must be recovered from within the same call —
    /// clear the cache, re-initialize, retry — rather than replaying the dead
    /// id on every subsequent call until `llmenv` is restarted.
    #[tokio::test]
    async fn call_tool_recovers_from_stale_session_after_one_retry() {
        let init_response = |sid: &str| {
            ResponseTemplate::new(200)
                .insert_header("mcp-session-id", sid)
                .set_body_json(serde_json::json!({
                    "jsonrpc": "2.0", "id": 0, "result": { "protocolVersion": "2025-06-18" }
                }))
        };
        let server = MockServer::start().await;
        // First initialize hands out the session the cache starts with.
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(init_response("sess-stale"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Re-initialize after the cache is cleared hands out a fresh one.
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(init_response("sess-fresh"))
            .mount(&server)
            .await;
        // The stale id is rejected — mcp-proxy's own "session not found" shape.
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/call"))
            .and(wiremock::matchers::header("mcp-session-id", "sess-stale"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // The re-initialized id succeeds.
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/call"))
            .and(wiremock::matchers::header("mcp-session-id", "sess-fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "content": [{ "type": "text", "text": "recovered" }] }
            })))
            .mount(&server)
            .await;

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let text = client
            .call_tool("icm_memory_recall", serde_json::json!({}))
            .await
            .expect("call_tool must recover after one retry");
        assert_eq!(text, "recovered");
    }

    /// A second stale-session rejection (even after the retry re-initialized)
    /// is a real error, not retried again — otherwise a server that's
    /// genuinely down would loop forever instead of surfacing a failure.
    #[tokio::test]
    async fn call_tool_gives_up_after_second_stale_session_rejection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("initialize"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("mcp-session-id", "sess-always-stale")
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 0, "result": { "protocolVersion": "2025-06-18" }
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(JsonRpcMethod("tools/call"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client =
            McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("valid URL");
        let err = client
            .call_tool("icm_memory_recall", serde_json::json!({}))
            .await
            .expect_err("a second rejection must not be retried again");
        assert!(err.to_string().contains("404"));

        let requests = server.received_requests().await.expect("received requests");
        let tools_calls = requests
            .iter()
            .filter(|r| wiremock::Match::matches(&JsonRpcMethod("tools/call"), r))
            .count();
        assert_eq!(
            tools_calls, 2,
            "exactly one retry: the original attempt plus one re-initialized retry"
        );
    }

    #[tokio::test]
    async fn call_tool_errors_on_unreachable() {
        // Valid public IP that should reject (no listening service)
        let client =
            McpHttpClient::new("https://8.8.8.8:0".to_string(), Duration::from_millis(200))
                .expect("valid URL");
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn new_rejects_plain_http_to_a_public_address() {
        let err = McpHttpClient::new("http://8.8.8.8:9092/mcp".into(), Duration::from_secs(2))
            .expect_err("cleartext to a public address must be refused")
            .to_string();
        assert!(err.contains("http://8.8.8.8:9092/mcp"), "{err}");
        assert!(err.contains("https://"), "{err}");
    }

    #[test]
    fn new_error_for_plain_http_hides_credentials_and_query() {
        let err = McpHttpClient::new(
            "http://user:pw@8.8.8.8:9/mcp?token=secret".into(),
            Duration::from_secs(2),
        )
        .expect_err("public http")
        .to_string();
        assert!(!err.contains("pw") && !err.contains("secret"), "{err}");
    }

    #[test]
    fn new_allows_plain_http_to_loopback_and_private_addresses() {
        for url in [
            "http://127.0.0.1:9092",
            "http://[::1]:9092",
            "http://10.1.2.3:9092",
            "http://192.168.1.5:9092",
            "http://172.16.0.9:9092",
            "http://[fd00::1]:9092",
            "http://100.64.0.7:9092",
            "http://localhost:9092",
        ] {
            McpHttpClient::new(url.into(), Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("{url} must be allowed: {e}"));
        }
    }

    #[test]
    fn new_allows_https_to_a_public_address() {
        McpHttpClient::new("https://8.8.8.8:443/mcp".into(), Duration::from_secs(2))
            .expect("https to a public address is allowed");
    }

    #[test]
    fn new_rejects_a_malformed_url() {
        assert!(McpHttpClient::new("http//not a url".into(), Duration::from_secs(2)).is_err());
    }

    #[test]
    fn invalid_url_errors_do_not_echo_credentials_or_query() {
        for url in [
            "http://user:pw@:9/mcp?token=secret",
            "ftp://user:pw@host/?token=secret",
        ] {
            let err = format!(
                "{:#}",
                McpHttpClient::new(url.into(), Duration::from_secs(2)).expect_err(url)
            );
            assert!(!err.contains("pw") && !err.contains("secret"), "{err}");
        }
    }

    #[test]
    fn plain_http_error_names_the_host() {
        let err = McpHttpClient::new("http://8.8.8.8:9/mcp".into(), Duration::from_secs(2))
            .expect_err("public http")
            .to_string();
        assert!(err.contains("8.8.8.8"), "{err}");
    }

    #[test]
    fn redact_url_strips_userinfo_query_and_fragment() {
        assert_eq!(redact_url("http://u:p@h:1/x?q=1#f"), "http://h:1/x");
        assert_eq!(redact_url("not a url"), "(invalid URL)");
    }

    #[test]
    fn require_https_fails_closed_on_an_unparseable_url() {
        let addr: SocketAddr = "8.8.8.8:80".parse().unwrap();
        assert!(require_https_for_public("not a url", &[addr]).is_err());
    }

    #[test]
    fn cleartext_check_judges_wrapped_ipv4_in_ipv6() {
        use std::net::{IpAddr, Ipv6Addr};
        let ip = |s: &str| IpAddr::V6(s.parse::<Ipv6Addr>().unwrap());
        assert!(!is_cleartext_safe(&ip("::ffff:8.8.8.8")));
        assert!(is_cleartext_safe(&ip("::ffff:10.0.0.1")));
        assert!(!is_cleartext_safe(&ip("64:ff9b::808:808")));
        assert!(!is_cleartext_safe(&ip("2002:808:808::1")));
        assert!(!is_cleartext_safe(&ip("fe80::1")));
    }

    #[test]
    fn embedded_ipv4_extracts_the_exact_wrapped_address() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let got = |s: &str| embedded_ipv4(&s.parse::<Ipv6Addr>().unwrap());
        let want = Some(Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(got("::ffff:1.2.3.4"), want);
        assert_eq!(got("::1.2.3.4"), want);
        assert_eq!(got("64:ff9b::102:304"), want);
        assert_eq!(got("2002:102:304::"), want);
    }

    #[test]
    fn embedded_ipv4_ignores_addresses_that_only_look_wrapped() {
        use std::net::Ipv6Addr;
        let got = |s: &str| embedded_ipv4(&s.parse::<Ipv6Addr>().unwrap());
        for s in [
            "::",
            "::1",
            "64:1::1",
            "1:ff9b::1",
            "64:ff9b:0:0:1::1",
            "2001:db8::1",
        ] {
            assert_eq!(got(s), None, "{s}");
        }
    }

    #[test]
    fn blocked_reason_sees_metadata_address_behind_nat64_6to4_and_compat() {
        use std::net::{IpAddr, Ipv6Addr};
        for s in [
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::",
            "::169.254.169.254",
        ] {
            let ip = IpAddr::V6(s.parse::<Ipv6Addr>().unwrap());
            assert!(
                blocked_reason(&ip, SsrfPolicy::AllowPrivateNetwork).is_some(),
                "{s}"
            );
        }
        let public = IpAddr::V6("64:ff9b::808:808".parse::<Ipv6Addr>().unwrap());
        assert!(blocked_reason(&public, SsrfPolicy::AllowPrivateNetwork).is_none());
        let unspecified = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        assert!(blocked_reason(&unspecified, SsrfPolicy::AllowPrivateNetwork).is_some());
    }

    proptest::proptest! {
        #[test]
        fn prop_cleartext_safe_matches_std_classification_for_ipv4(octets in proptest::array::uniform4(0u8..)) {
            let ip = std::net::Ipv4Addr::from(octets);
            let cgnat = octets[0] == 100 && (64..=127).contains(&octets[1]);
            let expected = ip.is_loopback() || ip.is_private() || cgnat;
            proptest::prop_assert_eq!(is_cleartext_safe(&IpAddr::V4(ip)), expected);
        }

        #[test]
        fn prop_redact_url_is_idempotent_and_hides_secrets(
            user in "[a-z]{1,8}", pw in "[A-Z]{6,10}", q in "[0-9]{6,10}",
        ) {
            let url = format!("http://{user}:{pw}@example.com/mcp?token={q}#frag");
            let once = redact_url(&url);
            proptest::prop_assert!(!once.contains(&pw) && !once.contains(&q));
            proptest::prop_assert_eq!(redact_url(&once), once);
        }
    }

    #[test]
    fn cleartext_safe_edges_for_cgnat() {
        use std::net::{IpAddr, Ipv4Addr};
        for ok in [
            Ipv4Addr::new(100, 64, 0, 1),
            Ipv4Addr::new(100, 127, 255, 254),
        ] {
            assert!(is_cleartext_safe(&IpAddr::V4(ok)), "{ok}");
        }
        for bad in [
            Ipv4Addr::new(100, 63, 255, 255),
            Ipv4Addr::new(100, 128, 0, 1),
        ] {
            assert!(!is_cleartext_safe(&IpAddr::V4(bad)), "{bad}");
        }
    }

    /// Test convenience: `SsrfPolicy::PublicOnly` with a generous timeout,
    /// matching every pre-existing test's expectations before `SsrfPolicy` and
    /// per-call DNS timeouts were introduced. 10s, not 2s (#1236): a real DNS
    /// lookup (`validate_url_accepts_public_ips`'s `example.com`) contending
    /// with the rest of a `--workspace` parallel test run for CPU/network
    /// blew a 2s budget intermittently — this is test-only slack, not a
    /// change to production's own timeout policy.
    fn validate(url: &str) -> anyhow::Result<(String, Vec<SocketAddr>)> {
        validate_url_production(url, SsrfPolicy::PublicOnly, Duration::from_secs(10))
    }

    #[test]
    fn validate_url_rejects_loopback() {
        assert!(validate("http://127.0.0.1:8080").is_err());
        assert!(validate("http://[::1]:8080").is_err());
    }

    #[test]
    fn validate_url_rejects_private_ips() {
        assert!(validate("http://10.0.0.1:8080").is_err());
        assert!(validate("http://192.168.1.1:8080").is_err());
        assert!(validate("http://172.16.0.1:8080").is_err());
    }

    #[test]
    fn validate_url_accepts_public_ips() {
        assert!(validate("http://8.8.8.8:8080").is_ok());
        assert!(validate("https://example.com:8080").is_ok());
    }

    #[test]
    fn validate_url_rejects_ipv6_ula() {
        // IPv6 Unique Local Addresses (fc00::/7) are private and must be rejected
        // under PublicOnly (#191). Covers both halves of the prefix: fc00::/8 and
        // fd00::/8.
        assert!(validate("http://[fc00::1]:8080").is_err());
        assert!(validate("http://[fd00::1]:8080").is_err());
        assert!(validate("http://[fd12:3456:789a::1]:8080").is_err());
    }

    #[test]
    fn validate_url_accepts_public_ipv6() {
        // Documentation range 2001:db8::/32 and a real public resolver address are
        // not in any blocked range, so they must pass.
        assert!(validate("http://[2001:db8::1]:8080").is_ok());
        assert!(validate("http://[2606:4700:4700::1111]:8080").is_ok());
    }

    #[test]
    fn validate_url_rejects_ipv4_mapped_ipv6_loopback() {
        // An attacker can smuggle a blocked IPv4 through the v6 namespace as a
        // mapped address (::ffff:127.0.0.1). The blocklist must unwrap and reject
        // it rather than treat the v6 wrapper as public (#191).
        assert!(validate("http://[::ffff:127.0.0.1]:8080").is_err());
        assert!(validate("http://[::ffff:169.254.169.254]:8080").is_err());
        assert!(validate("http://[::ffff:10.0.0.1]:8080").is_err());
    }

    #[test]
    fn validate_url_rejects_unspecified_and_metadata() {
        // 0.0.0.0 / :: route to localhost on many stacks; 169.254.169.254 is the
        // cloud metadata endpoint — both are classic SSRF targets (#191). Blocked
        // under both policies since neither has a legitimate memory-backend use.
        assert!(validate("http://0.0.0.0:8080").is_err());
        assert!(validate("http://[::]:8080").is_err());
        assert!(validate("http://169.254.169.254:8080").is_err());
        assert!(
            validate_url_production(
                "http://169.254.169.254:8080",
                SsrfPolicy::AllowPrivateNetwork,
                Duration::from_secs(2)
            )
            .is_err()
        );
    }

    #[test]
    fn validate_url_allow_private_network_accepts_loopback_and_private() {
        // The ICM memory/transcript backend is expected to run on loopback or
        // the operator's LAN (AGENTS.md), so its client must not reject the
        // exact topology it's documented to support.
        for url in [
            "http://127.0.0.1:9092",
            "http://[::1]:9092",
            "http://10.0.0.1:9092",
            "http://192.168.1.1:9092",
            "http://172.16.0.1:9092",
            "http://[fc00::1]:9092",
        ] {
            assert!(
                validate_url_production(
                    url,
                    SsrfPolicy::AllowPrivateNetwork,
                    Duration::from_secs(2)
                )
                .is_ok(),
                "expected {url} to be allowed under AllowPrivateNetwork"
            );
        }
    }

    #[test]
    fn validate_url_allow_private_network_still_rejects_link_local_and_special() {
        // Link-local (incl. the 169.254.169.254 cloud metadata address),
        // unspecified, and broadcast addresses have no legitimate use as a
        // memory backend regardless of trust policy.
        for url in [
            "http://169.254.169.254:9092",
            "http://[fe80::1]:9092",
            "http://0.0.0.0:9092",
            "http://[::]:9092",
        ] {
            assert!(
                validate_url_production(
                    url,
                    SsrfPolicy::AllowPrivateNetwork,
                    Duration::from_secs(2)
                )
                .is_err(),
                "expected {url} to still be rejected under AllowPrivateNetwork"
            );
        }
    }

    fn sock(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 9092)
    }

    #[test]
    fn domain_allow_private_drops_link_local_and_keeps_routable() {
        let resolved = vec![
            sock("fe80::c0:d5a8:4dfd:30bd"),
            sock("192.168.211.42"),
            sock("169.254.169.254"),
            sock("fe80::1"),
        ];
        let kept = drop_link_local_for_domain(
            resolved,
            SsrfPolicy::AllowPrivateNetwork,
            "http://still.local:9092/mcp",
        )
        .unwrap();
        assert_eq!(kept, vec![sock("192.168.211.42")]);
    }

    #[test]
    fn domain_allow_private_rejects_when_only_link_local_remains() {
        let err = drop_link_local_for_domain(
            vec![sock("fe80::1"), sock("169.254.169.254")],
            SsrfPolicy::AllowPrivateNetwork,
            "http://still.local:9092/mcp",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only to link-local"), "{err}");
    }

    #[test]
    fn domain_public_only_keeps_link_local_so_the_caller_rejects() {
        let resolved = vec![sock("93.184.216.34"), sock("fe80::1")];
        let kept =
            drop_link_local_for_domain(resolved.clone(), SsrfPolicy::PublicOnly, "http://x.test/")
                .unwrap();
        assert_eq!(kept, resolved);
    }

    #[test]
    fn domain_allow_private_still_rejects_unspecified_alongside_routable() {
        let kept = drop_link_local_for_domain(
            vec![sock("0.0.0.0"), sock("192.168.1.5")],
            SsrfPolicy::AllowPrivateNetwork,
            "http://x.test/",
        )
        .unwrap();
        // Not link-local, so it survives the drop and the caller's strict check rejects it.
        assert!(
            kept.iter()
                .any(|a| blocked_reason(&a.ip(), SsrfPolicy::AllowPrivateNetwork).is_some())
        );
    }

    #[test]
    fn domain_with_no_addresses_is_an_error() {
        assert!(
            drop_link_local_for_domain(vec![], SsrfPolicy::AllowPrivateNetwork, "http://x.test/")
                .is_err()
        );
    }

    #[test]
    fn blocked_reason_flags_private_and_special_ranges() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        // The blocklist is the single source of truth shared by literal-IP and
        // resolved-domain validation; assert it directly (#191).
        let blocked: &[IpAddr] = &[
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
        ];
        for ip in blocked {
            assert!(
                blocked_reason(ip, SsrfPolicy::PublicOnly).is_some(),
                "expected {ip} to be blocked under PublicOnly"
            );
        }
        let allowed: &[IpAddr] = &[
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
        ];
        for ip in allowed {
            assert!(
                blocked_reason(ip, SsrfPolicy::PublicOnly).is_none(),
                "expected {ip} to be allowed under PublicOnly"
            );
            assert!(
                blocked_reason(ip, SsrfPolicy::AllowPrivateNetwork).is_none(),
                "expected {ip} to be allowed under AllowPrivateNetwork"
            );
        }
    }

    #[test]
    fn validate_url_returns_pinned_addrs_for_literal_ip() {
        // The TOCTOU fix pins reqwest to the addresses validation already vetted.
        // For a literal IP, the returned set is exactly that address, and the host
        // key matches the literal so the caller can pin without re-parsing (#191).
        let (host, addrs) = validate("http://8.8.8.8:8080").expect("public IP ok");
        assert_eq!(host, "8.8.8.8");
        assert!(
            addrs
                .iter()
                .any(|a| a.ip().to_string() == "8.8.8.8" && a.port() == 8080)
        );
    }

    #[test]
    fn validate_url_returns_unbracketed_host_for_literal_ipv6() {
        // resolve_to_addrs keys on the unbracketed host; a re-parse via host_str
        // would yield the bracketed form and pin the wrong key. Returning the host
        // from the validating parse keeps the two in lockstep (#191).
        let (host, addrs) = validate("http://[2606:4700:4700::1111]:8080").expect("public IPv6 ok");
        assert_eq!(host, "2606:4700:4700::1111");
        assert!(addrs.iter().any(|a| a.port() == 8080));
    }

    #[test]
    fn validate_url_rejects_domain_resolving_to_loopback() {
        // DNS-rebinding TOCTOU: a hostname that resolves to a blocked address must
        // be rejected at validation time, before any request is sent. localhost is
        // the always-available stand-in for an attacker-controlled rebind (#191).
        assert!(validate("http://localhost:8080").is_err());
    }

    #[test]
    fn validate_url_rejects_unsupported_schemes() {
        assert!(validate("file:///tmp/socket").is_err());
        assert!(validate("ftp://example.com").is_err());
    }

    #[test]
    fn run_with_timeout_errors_when_exceeded() {
        // The closure sleeps far longer than the timeout, so the race is
        // decided by the sleep vs. timeout durations rather than by real DNS
        // resolution latency, which varies with scheduling and resolver
        // caching and made this test flaky under slow (e.g. coverage) runs.
        let result = run_with_timeout(
            || {
                thread::sleep(Duration::from_millis(50));
                42
            },
            Duration::from_millis(1),
        );
        assert_eq!(result, Err(mpsc::RecvTimeoutError::Timeout));
    }

    #[test]
    fn resolve_with_timeout_resolves_within_budget() {
        let addrs = resolve_with_timeout("localhost", 80, Duration::from_secs(5))
            .expect("localhost must resolve well within 5s");
        assert!(!addrs.is_empty());
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

    use proptest::prelude::*;

    fn any_policy() -> impl Strategy<Value = SsrfPolicy> {
        prop_oneof![
            Just(SsrfPolicy::PublicOnly),
            Just(SsrfPolicy::AllowPrivateNetwork),
        ]
    }

    proptest! {
        #[test]
        fn prop_blocked_reason_never_panics(octets in any::<[u8; 16]>(), v4 in any::<[u8; 4]>(), policy in any_policy()) {
            // The SSRF gate must total over every possible address, under either
            // policy (#191).
            let _ = blocked_reason(&IpAddr::V6(std::net::Ipv6Addr::from(octets)), policy);
            let _ = blocked_reason(&IpAddr::V4(std::net::Ipv4Addr::from(v4)), policy);
        }

        #[test]
        fn prop_is_unique_local_v6_matches_fc00_slash_7(octets in any::<[u8; 16]>()) {
            // The hand-rolled prefix test must agree with the fc00::/7 definition:
            // first byte 0xfc or 0xfd (the unstable std is_unique_local).
            let v6 = std::net::Ipv6Addr::from(octets);
            let expected = matches!(octets[0], 0xfc | 0xfd);
            prop_assert_eq!(is_unique_local_v6(&v6), expected);
        }

        #[test]
        fn prop_ula_v6_always_blocked_under_public_only(first in prop_oneof![Just(0xfcu8), Just(0xfdu8)], rest in any::<[u8; 15]>()) {
            // Every fc00::/7 address is rejected under PublicOnly, regardless of
            // the lower bits. Treated like RFC 1918 private v4 under
            // AllowPrivateNetwork, so not asserted here (see
            // validate_url_allow_private_network_accepts_loopback_and_private).
            let mut octets = [0u8; 16];
            octets[0] = first;
            octets[1..].copy_from_slice(&rest);
            let v6 = std::net::Ipv6Addr::from(octets);
            prop_assert!(blocked_reason(&IpAddr::V6(v6), SsrfPolicy::PublicOnly).is_some());
        }

        #[test]
        fn prop_ipv4_mapped_v6_judged_as_v4(v4 in any::<[u8; 4]>(), policy in any_policy()) {
            // ::ffff:a.b.c.d must yield the same verdict as a.b.c.d — a blocked v4
            // range cannot be smuggled through the v6 namespace — under either
            // policy.
            let v4_addr = std::net::Ipv4Addr::from(v4);
            let mapped = v4_addr.to_ipv6_mapped();
            prop_assert_eq!(
                blocked_reason(&IpAddr::V6(mapped), policy).is_some(),
                blocked_reason(&IpAddr::V4(v4_addr), policy).is_some()
            );
        }

        #[test]
        fn prop_only_400_and_404_are_stale_session_status(code in 100u16..1000) {
            // #1094: exactly 400/404 trigger the clear-and-retry path; every
            // other status (2xx included, though callers only reach this on
            // a non-2xx) must not.
            let Ok(status) = reqwest::StatusCode::from_u16(code) else {
                return Ok(());
            };
            let expected = matches!(code, 400 | 404);
            prop_assert_eq!(is_stale_session_status(status), expected);
        }
    }
}
