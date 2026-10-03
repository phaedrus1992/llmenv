# Issue #2148 — report MCP text that Claude Code cuts at 2,048 characters

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2148
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (doctor check), plus one small change in consolidation
- **Note:** an earlier version of this spec depended on #2160 (an `rmcp`-based client). #2160 moved to v3.13.0, so this spec builds on the existing hand-rolled `McpHttpClient`. #2160 may replace the transport later without changing the doctor output.

This is a spec, not a plan.

## Problem

Claude Code cuts each MCP tool description and each MCP server's `initialize` instructions to 2,048 characters before it sends them to the model.
`CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` changes the limit (Claude Code 2.1.280 or later).
The cut is silent.
llmenv wires MCP servers (ICM, codebase-memory, user entries) and has no way to tell a user that a server's guidance is being cut.

Separately, post-session consolidation runs `claude -p` (`call_claude` in `src/consolidation/mod.rs`).
That prompt uses no tools, but the first turn of a non-interactive session waits for MCP servers to connect.
`CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` (Claude Code 2.1.274 or later) skips that wait.

## Claude Code facts (env-vars reference, fetched 2026-10-02)

| Variable | Meaning |
| --- | --- |
| `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` | maximum characters of each MCP tool description and each server's instructions; default 2048; accepts a positive whole number in plain digits; anything else is ignored and the default applies |
| `CLAUDE_CODE_MCP_STARTUP_WAIT_MS` | how long the first turn of a non-interactive session waits for connecting MCP servers; `0` skips the wait; a `--permission-prompt-tool` server keeps its own `MCP_TIMEOUT` wait |

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `McpHttpClient { url, client, session_id }` with `new(url, timeout)`, `probe()`, `call_tool(name, args)`; `ensure_session` sends `initialize` and reads only the `Mcp-Session-Id` header, discarding the result body | `src/hook_run/mcp_client.rs` |
| SSRF rules live in `validate_url_production`; `test_new` bypasses them for tests | `src/hook_run/mcp_client.rs` |
| `ResolvedMcp.kind` is `Stdio { command, args, env }` or `Remote { transport: Http | Sse, url }`, with `headers` | `src/mcp/resolve.rs` |
| `mcp_health::probe_stdio` starts a stdio server with `kill_on_drop`, performs the `initialize` handshake over its stdio, and kills it; `managed_servers()` lists ICM plus codebase-memory | `src/hook_run/mcp_health.rs` |
| Doctor: `run_doctor_mcp_servers` is the template section; `effective_token_efficiency_var(native_claude_env, key)` reads an env var from the process or `native.claude_code.env` | `src/cli/doctor.rs` |
| `wiremock` is a dev dependency | `Cargo.toml` |
| `Command::Doctor` has `gc` and `all` flags; `run_doctor(gc, all, use_color)` | `src/cli/mod.rs`, `src/cli/doctor.rs` |
| `call_claude` builds and spawns the `claude -p` command for consolidation | `src/consolidation/mod.rs` |

## Decisions

1. **Count characters, not bytes.** The limit is in characters. Use `str::chars().count()`.
2. **HTTP servers by default; stdio servers only on request.**
   Probing a stdio server starts its process, which can have side effects (codebase-memory starts its daemon).
   `llmenv doctor` probes `Remote` servers with transport `Http` every time.
   It probes `Stdio` servers only with a new flag, `llmenv doctor --probe-mcp`.
   `Sse` servers are not probed; print one `{info}` line saying so.
3. **Read-only.** The probe calls only `initialize` and `tools/list`. It never calls a tool.
4. **A failed probe is information, not a warning.** A server can be down while doctor runs.
5. **Extend the existing client; do not add a dependency.**
   `McpHttpClient` gains two read-only methods.
   The stdio path reuses the `probe_stdio` handshake and adds a `tools/list` request over the same pipes.
6. **Paginate `tools/list`.** Follow `nextCursor` until it is absent, with a cap of 50 pages so a misbehaving server cannot loop doctor.

## Design

### Limit

```rust
/// Claude Code's default cut for MCP tool descriptions and server instructions.
const CLAUDE_MCP_TEXT_LIMIT: usize = 2048;
```

Effective limit: `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` from the process environment, else from `native.claude_code.env`, when it is 1 to 9 ASCII digits and not zero; otherwise `CLAUDE_MCP_TEXT_LIMIT`.
Read it with `effective_token_efficiency_var`.

### Client (`src/hook_run/mcp_client.rs`)

- `ensure_session` keeps the parsed `initialize` result (`serverInfo`, `instructions`) in a new `Option<InitializeInfo>` beside the cached session id.
- `pub async fn initialize_info(&self) -> anyhow::Result<InitializeInfo>`: drops the cached session (like `probe`), runs `ensure_session`, returns the stored info.
- `pub async fn list_tools(&self) -> anyhow::Result<Vec<ToolSummary>>` with `ToolSummary { name, description: Option<String> }`: posts `tools/list` with the session header, follows `nextCursor`, uses the same stale-session retry as `call_tool` (factor the request send into a helper both use so there is one JSON-RPC path).
- `pub(crate) fn with_headers(url, timeout, headers)` constructor so a `Remote` entry's `headers` reach the probe.

### Probe (`src/mcp/probe.rs`, new)

```rust
pub(crate) struct McpTextReport {
    pub server: String,
    pub instructions_chars: Option<usize>,
    /// (tool name, description chars), in the server's order.
    pub tools: Vec<(String, usize)>,
}

pub(crate) async fn probe(mcp: &ResolvedMcp, timeout: Duration) -> anyhow::Result<McpTextReport>;
```

- `Remote { Http }`: `McpHttpClient::with_headers`, then `initialize_info` and `list_tools`.
- `Stdio`: a `probe_stdio`-style child with `kill_on_drop`; after the handshake, write a `tools/list` request line and read until the matching `id` reply, paginating as above; cancel and kill the child.
  Put the line-delimited JSON-RPC send/receive in `mcp_health` as a shared helper so the health probe and this probe stay one implementation.
- Other kinds: `Err` with `transport <kind> is not probed`.
- A tool with no description counts as 0.
- Timeout per server: 5 seconds, covering connect and both calls.
- `fn measure(instructions: Option<&str>, tools: &[ToolSummary]) -> McpTextReport` is pure, so the counting is testable without a server.

### Doctor output

Add `probe_mcp: bool` to `Command::Doctor` (`#[arg(long)]`, help `Also start stdio MCP servers to measure their instructions and tool descriptions`) and pass it to `run_doctor` (4 positional parameters after the change, inside the limit of 5).

After the retired-settings section (#2145), when the Claude Code adapter is installed and the manifest has MCP servers, print `MCP text limits (Claude Code keeps <limit> characters):`, then one block per server in manifest order:

- Probe failed: `{info} <server>: not measured (<error>)`.
- Not probed (stdio without `--probe-mcp`): `{info} <server>: stdio server not started; run llmenv doctor --probe-mcp to measure it`.
- Instructions over the limit: `{warn} <server>: instructions are <n> characters; Claude Code sends the first <limit>`.
- Each tool over the limit: `{warn} <server>/<tool>: description is <n> characters; Claude Code sends the first <limit>`.
- Nothing over the limit: `{pass} <server>: instructions <n or "none">, <k> tools, longest description <m> characters`.

Run the probes concurrently with `tokio::task::JoinSet` (no `futures` dependency), keep each result with its manifest index, then print in manifest order.
The formatting is a pure function over `Vec<Result<McpTextReport>>`.

### Consolidation

In `call_claude` (`src/consolidation/mod.rs`), set `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` on the command before spawning.
Comment: the consolidation prompt calls no tools, so waiting for MCP servers only adds start-up time.
Older Claude Code versions ignore the variable.

## Tests

1. Limit parsing: unset, `"4096"`, `"0"`, `"abc"`, `"4096 "`, `"99999999999"` (more than 9 digits).
2. Character counting: a 2,048-character string of multi-byte characters is not over the limit; 2,049 is.
3. `initialize_info` and `list_tools` against `wiremock`: instructions returned; two pages of tools concatenated in order; a server with no `nextCursor` gives one page; a stale-session 404 on `tools/list` re-initializes once.
4. Pagination cap: a server that always returns a cursor stops after 50 pages with an error naming the cap.
5. `measure` on synthetic input (pure).
6. Stdio probe against a small stub server script on `PATH` (a shell or Python script that answers `initialize` and two pages of `tools/list` on stdin/stdout): the report matches and no child process is left (check with the pid the test captured).
7. Doctor output formatting from a synthetic report list (pure; no network).
8. `call_claude` command test: the built command has `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` in its environment (extract command building into a function if it is not one already).

## Acceptance criteria

1. `llmenv doctor` with an ICM HTTP server configured prints a line for it with the instruction and tool counts.
2. `llmenv doctor --probe-mcp` also measures codebase-memory, and no `codebase-memory-mcp` child process is left running afterwards.
3. Changelog `Added`: doctor reports MCP text that Claude Code cuts. `Changed`: consolidation's `claude -p` call no longer waits for MCP servers.
4. `website/docs/commands.md` doctor section documents the check and `--probe-mcp`, tagged `(added in v3.12.0)`.

## Out of scope

- Shortening any server's text. The fix belongs to the server's owner.
- Probing `Sse` servers.
- Replacing the HTTP client with `rmcp` (#2160, v3.13.0).
