//! Minimal HTTP JSON-RPC MCP client — only the `tools/call` path this feature
//! needs. Not a general MCP library.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use url::{Host, Url};

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
}

impl McpHttpClient {
    /// Build a client for `url` whose every request is bounded by `timeout`.
    ///
    /// # Errors
    /// Returns an error if the URL is invalid, uses an unsupported scheme, or
    /// points to a private/loopback IP address (SSRF protection).
    pub fn new(url: String, timeout: Duration) -> anyhow::Result<Self> {
        // Resolve and SSRF-validate up front, then pin reqwest to exactly the
        // vetted addresses. Pinning closes the DNS-rebinding TOCTOU: reqwest never
        // performs its own (re-)resolution at send() time, so a hostname cannot
        // resolve to a public IP during validation and a private one at connect
        // time — the connection can only target an address we already approved.
        let (host, addrs) =
            validate_url_production(&url, SsrfPolicy::AllowPrivateNetwork, timeout)?;
        // Pin unconditionally to the host/addrs the SSRF check just vetted.
        // validation already guaranteed a non-empty host, so there is no
        // fall-through path where reqwest could re-resolve at send() time.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .resolve_to_addrs(&host, &addrs)
            .build()
            .context("failed to build HTTP client (TLS backend unavailable)")?;
        Ok(Self {
            url,
            client,
            session_id: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
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
        })
    }

    /// Negotiate an MCP session if one hasn't already been established on this
    /// client: send `initialize`, capture the `Mcp-Session-Id` response header
    /// (if the server sends one — plain non-session-scoped servers won't), and
    /// best-effort acknowledge with `notifications/initialized`.
    ///
    /// # Errors
    /// Network failure or a non-2xx status on the `initialize` request itself.
    async fn ensure_session(&self) -> anyhow::Result<Option<String>> {
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
            .with_context(|| format!("POST {} for MCP initialize", self.url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "(failed to read error body)".to_string());
            return Err(anyhow!("MCP initialize returned HTTP {status}: {body}"));
        }
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

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

    /// Call one MCP tool and return the concatenated text content.
    ///
    /// # Errors
    /// Network failure, session negotiation failure, timeout, non-2xx status, a
    /// JSON-RPC `error` field, or a response missing `result.content[].text`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<String> {
        let session_id = self.ensure_session().await?;

        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
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
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return blocked_reason(&IpAddr::V4(mapped), policy);
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
fn resolve_with_timeout(
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

/// Whether an IPv6 address falls in the Unique Local Address range `fc00::/7`.
///
/// `Ipv6Addr::is_unique_local` is unstable, so test the prefix directly: the
/// top seven bits are `1111110`, i.e. the first byte is `0xfc` or `0xfd` (#191).
fn is_unique_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    (v6.octets()[0] & 0xfe) == 0xfc
}

/// Validate ICM backend URL to prevent SSRF attacks and return the host string
/// together with the vetted set of socket addresses the connection may target.
///
/// Rejects unsupported schemes, blocked literal IPs, and — for hostnames —
/// resolves DNS and rejects the URL if *any* resolved address is blocked. The
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
    let parsed = Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!(
            "unsupported URL scheme '{}' (only http/https allowed)",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host()
        .ok_or_else(|| anyhow!("URL {url} has no host"))?;
    // reqwest's resolve_to_addrs keys on the unbracketed host string; Host's
    // Display matches host_str without the IPv6 brackets, which is what we pin.
    let host_key = match host {
        Host::Ipv4(v4) => v4.to_string(),
        Host::Ipv6(v6) => v6.to_string(),
        Host::Domain(name) => name.to_string(),
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("URL {url} has no port and an unknown default"))?;

    let addrs: Vec<SocketAddr> = match host {
        Host::Ipv4(v4) => vec![SocketAddr::new(IpAddr::V4(v4), port)],
        Host::Ipv6(v6) => vec![SocketAddr::new(IpAddr::V6(v6), port)],
        Host::Domain(name) => resolve_with_timeout(name, port, timeout)?,
    };

    if addrs.is_empty() {
        return Err(anyhow!("host of URL {url} resolved to no addresses"));
    }

    // Reject if ANY resolved address is blocked. A permissive "some address is
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

    #[tokio::test]
    async fn call_tool_errors_on_unreachable() {
        // Valid public IP that should reject (no listening service)
        let client = McpHttpClient::new("http://8.8.8.8:0".to_string(), Duration::from_millis(200))
            .expect("valid URL");
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    /// Test convenience: `SsrfPolicy::PublicOnly` with a generous timeout,
    /// matching every pre-existing test's expectations before `SsrfPolicy` and
    /// per-call DNS timeouts were introduced.
    fn validate(url: &str) -> anyhow::Result<(String, Vec<SocketAddr>)> {
        validate_url_production(url, SsrfPolicy::PublicOnly, Duration::from_secs(2))
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
    }
}
