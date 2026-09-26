# Issue #2160 — replace the hand-written MCP HTTP client with rmcp

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2160
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** refactor with one behavior fix
- **Blocks:** #2148 (needs `tools/list` and server instructions), #2161 (uses the same crate)

This is a spec, not a plan.

## Problem

`src/hook_run/mcp_client.rs` (about 1,100 lines on `release/3.x`) implements the MCP Streamable HTTP client protocol by hand:
the `initialize` handshake, `notifications/initialized`, `Mcp-Session-Id` caching, one retry on a stale session (HTTP 400 or 404), JSON-RPC framing, and `tools/call` result parsing.
It implements only `tools/call`.
Any new need (`tools/list`, the server's `initialize` instructions) means more hand-written protocol code.

It also ignores `result.isError`: a tool that reports an error returns its error text to the caller as if it were a normal result.

## Decision

Use the official Rust MCP SDK, `rmcp` (repository `modelcontextprotocol/rust-sdk`, crate `rmcp`, version 3.4.1, published 2026-09-23, license Apache-2.0).
Keep llmenv's own SSRF validation and address pinning; `rmcp` accepts a caller-built `reqwest::Client`.

## rmcp facts (source at tag `rmcp-v3.4.1`)

| Fact | Location in `crates/rmcp/src` |
| --- | --- |
| `StreamableHttpClientTransport::with_client(client: C, config: StreamableHttpClientTransportConfig)` | `transport/streamable_http_client.rs` line 2028 |
| `impl StreamableHttpClient for reqwest::Client` | `transport/common/reqwest/streamable_http_client.rs` line 49 |
| `serve_client(...)` runs the handshake and returns a running client | `service/client.rs` line 693 |
| `list_tools`, `list_all_tools` | `service/client.rs` lines 1689, 1748 |
| `peer_info()` returns the server's `initialize` result (includes `instructions`) | `service.rs` line 1035 |
| `reqwest` dependency: `0.13.2`, `default-features = false`, features `json`, `stream` | `crates/rmcp/Cargo.toml` line 77 |
| Feature `transport-streamable-http-client-reqwest` = streamable client + `__reqwest` (no TLS backend chosen) | `crates/rmcp/Cargo.toml` line 164 |
| Feature `transport-child-process` = `tokio/process` + `process-wrap` | `crates/rmcp/Cargo.toml` line 177 |
| Workspace `rust-version = "1.88"`, `edition = "2024"` | `Cargo.toml` |

llmenv on `release/3.x` has `reqwest = "=0.13.5"` with `rustls`, `tokio = "=1.53.1"`, edition 2024, `rust-version = "1.95"`.
The versions are compatible; Cargo unifies `reqwest` to one 0.13 build with llmenv's `rustls` feature.

## Dependency

Add to the root `Cargo.toml` `[dependencies]`:

```toml
rmcp = { version = "=3.4.1", default-features = false, features = [
  "client",
  "transport-streamable-http-client-reqwest",
  "transport-child-process",
] }
```

- `default-features = false` drops `server`, `macros` and `base64`, which llmenv does not need here.
- Do not enable rmcp's `reqwest`, `reqwest-native-tls` or `reqwest-tls-no-provider` features; llmenv's own `reqwest` features pick the TLS backend.
- `transport-child-process` is for #2148 and #2161; add it now so there is one dependency change.

Then, in the same change:

1. `cargo deny check` must pass. If a new transitive crate brings a license not in `deny.toml` `[licenses].allow` and `about.toml` `accepted`, confirm it has no strong copyleft before you add it to both (AGENTS.md rule).
2. Run `scripts/gen-attribution.sh` and commit `THIRD-PARTY-LICENSES.md` and `website/docs/third-party-licenses.md`.

## Public API to keep

These callers must compile unchanged (verified on `release/3.x`):
`src/consolidation/mod.rs`, `src/hook_run/{action,detached_consolidation,detached_store,mod}.rs`, `src/memory/{mod,prune}.rs`, `src/session_log/{detached,dispatch}.rs`.

| Item | Keep |
| --- | --- |
| `McpHttpClient::new(url: String, timeout: Duration) -> anyhow::Result<Self>` | yes; still runs `validate_url_production` with `SsrfPolicy::AllowPrivateNetwork` and pins with `resolve_to_addrs` |
| `McpHttpClient::test_new(url, timeout)` (`cfg(test)`) | yes; no SSRF check |
| `async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<String>` | yes; returns all `text` content items joined exactly as `extract_text` does today |
| `validate_url_production` | yes, unchanged |
| `[LLMENV_MCP_CALL] <tool> <us>us` stderr trace under `LLMENV_TRACE_TIMING` | yes |
| `#[derive(Debug, Clone)]` on `McpHttpClient`; clones share one cached session through an `Arc` | yes; clones share one cached rmcp service through `Arc<tokio::sync::Mutex<Option<…>>>`, and `Debug` must not print the service |

New methods, needed by #2148:

```rust
/// Server `initialize` instructions, or `None` when the server sends none.
pub async fn server_instructions(&self) -> anyhow::Result<Option<String>>;
/// Every tool the server lists, following pagination.
pub async fn list_tools(&self) -> anyhow::Result<Vec<rmcp::model::Tool>>;
```

## Behavior

1. **Lazy connect.** The first call builds the transport with `StreamableHttpClientTransport::with_client(pinned_reqwest_client, config)` where `config` holds the URL, and calls `serve_client` with a minimal client handler (`()` or the SDK's default client info with name `llmenv` and the crate version).
   The running service is cached in the struct behind a `tokio::sync::Mutex<Option<…>>`, as the session id is today.
2. **Stale session.** If a call fails with an error that `rmcp` reports for HTTP 400 or 404 on an existing session, drop the cached service, connect again, and retry once. This keeps #1094's behavior.
   Match on the rmcp error type, not on message text. If rmcp does not expose the status, match on its transport-error variant for session expiry; record the mapping in a comment that cites the rmcp file and line.
3. **Timeout.** Store the `timeout` passed to `new` in the struct. Keep it on the `reqwest::Client` as today, and also wrap every call, including connect, in `tokio::time::timeout(self.timeout, …)`. A timeout is an error naming the tool and the timeout.
4. **`isError`.** A `tools/call` result with `is_error == Some(true)` returns `Err`, with message `tool <name> reported an error: <joined text>`. This is a behavior change: today such text is returned as a result.
5. **No text content.** A result with no `text` content item returns `Err("tool <name> response has no text content")`, as today.
6. **Errors.** Every error names the operation, the tool (when there is one) and the URL.

## Tests

Keep the existing `wiremock` tests in `mcp_client.rs`, `src/hook_run/mod.rs` (near line 5325) and `src/session_log/dispatch.rs` (near line 99).
They mock the HTTP exchange, so they test the new client against the same wire traffic.
Adjust a mock only when rmcp sends a request the old client did not (for example a different `Accept` header); keep every assertion about results and errors.

Add:

1. `isError: true` result gives `Err` containing the tool name and the text.
2. Stale session: first `tools/call` returns 404, the client re-initializes once and the second call succeeds; a second 404 gives `Err`.
3. `server_instructions` returns the `instructions` string from a mocked `initialize` result, and `None` when absent.
4. `list_tools` follows a `nextCursor` across two pages.
5. SSRF: `new` with a URL that fails `validate_url_production` returns the same error as today.

## Acceptance criteria

1. `mcp_client.rs` contains no hand-written JSON-RPC framing, `initialize` handshake or session-id header code.
2. All existing callers compile unchanged; all existing tests pass.
3. A real session against the maintainer's ICM server recalls and stores memories as before.
4. `cargo deny check` passes; attribution files are regenerated.
5. Changelog `Changed`: the MCP client uses the official Rust SDK. `Fixed`: an MCP tool error is no longer treated as a normal result.

## Out of scope

- The external `mcp-proxy` process (#2161).
- Any MCP server code in llmenv.
