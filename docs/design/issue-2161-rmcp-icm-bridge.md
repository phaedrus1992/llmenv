# Issue #2161 — built-in rmcp bridge for `icm serve`, replacing `mcp-proxy`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2161
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (removes an external dependency)
- **Depends on:** #2160 (adds `rmcp` 3.4.1); see `issue-2160-rmcp-client.md`

This is a spec, not a plan.

## Problem

When this host is the ICM server, `llmenv export` keeps an external Python program, `mcp-proxy` (github.com/sparfenyuk/mcp-proxy), running to expose `icm serve` (stdio) as MCP Streamable HTTP at `http://<addr>:<port>/mcp` for every agent on the LAN.
This needs Python tooling on the server host: `mcp-proxy` on `PATH`, or `uvx` to fetch it (about 2.1 s cold start against 0.55 s).
`mcp-proxy` must also stay pinned to `mcp<2`, because newer `mcp` releases break it.

`rmcp` has both halves needed to do this inside llmenv: a child-process client and a Streamable HTTP server.

## Verified facts

### llmenv (release/3.x)

| Fact | Location |
| --- | --- |
| Lifecycle (`ensure_running`, lock file, pidfile, bounded log, liveness = something serving the bind address) is generic over a `spawn: F` function | `src/mcp/proxy.rs` lines 97 to 460 |
| The only spawn function is `spawn_mcp_proxy(bind)`, which runs `<mcp-proxy or uvx mcp-proxy> --host <ip> --port <port> -- icm serve` detached | `src/mcp/proxy.rs` near line 791 |
| `mcp_proxy_command()` picks `mcp-proxy` on `PATH`, else `uvx mcp-proxy` | `src/mcp/proxy.rs` near line 710 |
| Caller: `llmenv export` path | `src/cli/mod.rs` near line 1049 |
| Pidfile: `$XDG_STATE_HOME/llmenv/mcp-proxy.pid`, else `~/.local/state/llmenv/mcp-proxy.pid` | `default_pid_path`, `src/mcp/proxy.rs` near line 481 |
| Clients connect to `http://<addr>:<port>/mcp` | `src/mcp/resolve.rs` line 243 |
| doctor requires `mcp-proxy` or `uvx` on `PATH` when a memory `server_host` is remote | `src/cli/doctor.rs` near line 333 |
| Docs describe the `mcp-proxy`/`uvx` requirement | `website/docs/mcp.md` lines 61 to 80, `website/docs/configuration.md` line 522 |
| `hyper` 1.11.1, `hyper-util` 0.1.21 and `tower` 0.5.3 are already in `Cargo.lock` (through `reqwest`) | `Cargo.lock` |

### rmcp (tag `rmcp-v3.4.1`)

| Fact | Location |
| --- | --- |
| `StreamableHttpService::new(factory, session_manager, config)` is a tower service | `crates/rmcp/src/transport/streamable_http_server/tower.rs` lines 1048, 1134 |
| Serving it on plain hyper without axum: `TowerToHyperService::new(StreamableHttpService::new(|| Ok(handler), LocalSessionManager::default().into(), Default::default()))`, then `hyper_util::server::conn::auto::Builder::new(TokioExecutor::default()).serve_connection(io, service)` per accepted connection | `examples/servers/src/counter_hyper_streamable_http.rs` |
| Child-process client transport: feature `transport-child-process` | `crates/rmcp/Cargo.toml` line 177 |
| Server features: `server`, `transport-streamable-http-server` | `crates/rmcp/Cargo.toml` lines 128, 186 |

## Decisions

1. **A hidden `llmenv` subcommand runs the bridge.** `llmenv mcp-bridge --host <ip> --port <port> -- <command> [args…]`, `#[command(hide = true)]`.
   The existing lifecycle code spawns it through a new spawn function; the lifecycle code does not change.
2. **Forward only what ICM uses, plus the lists that go with it.** Forward `initialize` data, tools, resources and prompts. Everything else gets rmcp's default method-not-found response.
3. **One child, many sessions.** The bridge starts one `icm serve` child at start-up and shares one rmcp client to it across all HTTP sessions. rmcp multiplexes concurrent requests by id.
4. **The bridge exits when the child exits.** The next `llmenv export` starts a new bridge through the existing liveness check. No restart loop inside the bridge.
5. **Serve `/mcp` only.** Any other path gets HTTP 404.
6. **No Python fallback.** Remove `mcp-proxy` and `uvx` support completely.

## Design

### Dependencies

Extend the `rmcp` entry from #2160 with features `server` and `transport-streamable-http-server`.
Add `hyper-util = { version = "=0.1.21", features = ["server-auto", "tokio", "service"] }` and, if the code names hyper types directly, `hyper = { version = "=1.11.1", features = ["server", "http1", "http2"] }`.
These pin the versions already in `Cargo.lock`, so they add server code paths but no new crates.
Run `cargo deny check` and `scripts/gen-attribution.sh` in the same change.

### Subcommand (`src/mcp/bridge.rs`, wired in `src/cli/mod.rs`)

1. Parse `--host` (IP address) and `--port` (1 to 65535), then the child command after `--`. Reject a missing child command with a clear error.
2. Start the child with rmcp's `TokioChildProcess` transport and `serve_client`. If the handshake fails within 10 seconds, exit with status 1 and an error naming the child command.
3. Read `peer_info()` from the child: server info, capabilities and instructions.
4. Build a forwarding handler type `IcmForwarder { upstream: Arc<RunningClient>, info: ServerInfo }` that implements rmcp's `ServerHandler`:
   - `get_info()`: return `info` (the child's server info, protocol version, capabilities and instructions, unchanged).
   - `list_tools`, `call_tool`: forward to `upstream` and return its result or error unchanged.
   - `list_resources`, `list_resource_templates`, `read_resource`, `list_prompts`, `get_prompt`: forward the same way, only when the child's capabilities include resources or prompts; otherwise leave rmcp's default.
5. Serve `StreamableHttpService::new(move || Ok(forwarder.clone()), LocalSessionManager::default().into(), Default::default())` on a `tokio::net::TcpListener` bound to `host:port`, wrapped so that a request whose path is not `/mcp` gets 404.
6. Exit status 0 on SIGTERM or SIGINT; status 1 when the child exits or the listener fails. On exit, cancel the upstream service so the child exits too.
7. Log to stderr; the lifecycle code already sends the child's stderr to the bounded log.

### Lifecycle changes (`src/mcp/proxy.rs`)

1. Replace `spawn_mcp_proxy` with `spawn_icm_bridge(bind)`: run `std::env::current_exe()` with `mcp-bridge --host <ip> --port <port> -- icm serve`, using the same `configure_detached`, detach and log handling as today.
2. Delete `mcp_proxy_command` and the `uvx` branch.
3. Rename the pidfile to `icm-bridge.pid` (same directory) and the log file to match.
4. **Migration.** Before `ensure_running`, if the old `mcp-proxy.pid` exists: read the pid; if `is_alive(pid) == Some(true)`, send SIGTERM and wait up to 2 seconds for the bind address to stop answering (`probe_tcp`); then delete the old pidfile and its lock file.
   Without this step the old proxy keeps the port, and the liveness check treats it as the bridge.
   Send the signal the way `is_alive` probes (near line 961): run `kill -TERM <pid>` through `std::process::Command`, Unix only, with the same pid range check. Do not add `libc` or `nix`.
   On non-Unix targets, skip the signal and print one warning that names the old pidfile and tells the user to stop `mcp-proxy` by hand.
5. Rename the module doc and user-facing strings from `mcp-proxy` to `ICM bridge`. Keep the file name `proxy.rs` to keep the diff small.

### doctor (`src/cli/doctor.rs` near line 333)

Replace the `mcp-proxy or uvx` check with a check that `icm` is on `PATH` when a memory `server_host` is this host, with the same pass and fail style.

## Tests

1. `mcp-bridge` argument parsing: missing `--`, bad port, IPv6 host.
2. Forwarder: start the bridge in-process against an in-memory rmcp server (in `[dev-dependencies]`) that has instructions and two tools. An rmcp client over HTTP to the bridge sees the same instructions and tools, and a tool call returns the upstream result.
3. Path: `GET /other` gives 404; `/mcp` works.
4. Child exit: when the upstream closes, the bridge future ends with an error.
5. Migration: with a fake old pidfile naming a dead pid, the pidfile is removed and no signal is sent; with a live pid (a spawned `sleep`), SIGTERM is sent.
6. Keep the existing `proxy.rs` lifecycle tests; they take `spawn` as a parameter and do not depend on `mcp-proxy`.

## Acceptance criteria

1. On the ICM server host with no `mcp-proxy` and no `uvx` installed, `llmenv export` starts the bridge, and agents on another host recall and store memories.
2. After upgrade, a host that ran `mcp-proxy` switches to the bridge on the next `llmenv export`, with no manual step.
3. `git grep -n 'uvx\|mcp-proxy' -- src website/docs ':!website/docs/changelog.md'` returns only the migration code and its comment.
4. Changelog `Changed`: llmenv serves ICM over HTTP itself; `mcp-proxy` and `uvx` are no longer needed. `Removed`: the `mcp-proxy`/`uvx` requirement.
5. `website/docs/mcp.md` describes the built-in bridge in place of `mcp-proxy`, tagged `(changed in v3.12.0)`.

## Out of scope

- Bridging servers other than `icm serve`.
- Authentication on the bridge. The network exposure and trust model are the same as with `mcp-proxy` today.
