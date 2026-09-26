# Issue #2148 — report MCP text that Claude Code cuts at 2,048 characters

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2148
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (doctor check), plus one small fix in consolidation
- **Depends on:** #2160 (`McpHttpClient::list_tools`, `server_instructions`, and the `rmcp` dependency); see `issue-2160-rmcp-client.md`

This is a spec, not a plan.

## Problem

Claude Code cuts each MCP tool description and each MCP server's `initialize` instructions to 2,048 characters before it sends them to the model.
`CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` changes the limit (Claude Code 2.1.280 or later).
The cut is silent.
llmenv wires MCP servers (ICM, codebase-memory, user entries) and has no way to tell a user that a server's guidance is being cut.

Separately, post-session consolidation runs `claude -p` (`call_claude`, `src/consolidation/mod.rs` near line 121).
That prompt uses no tools, but the first turn of a non-interactive session waits for MCP servers to connect.
`CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` (Claude Code 2.1.274 or later) skips that wait.

## Claude Code facts (env-vars reference, fetched 2026-09-26)

| Variable | Meaning |
| --- | --- |
| `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` | maximum characters of each MCP tool description and each server's instructions; default 2048; accepts a positive whole number in plain digits; anything else is ignored and the default applies |
| `CLAUDE_CODE_MCP_STARTUP_WAIT_MS` | how long the first turn of a non-interactive session waits for connecting MCP servers; `0` skips the wait |

## Decisions

1. **Count characters, not bytes.** The limit is in characters. Use `str::chars().count()`.
2. **HTTP servers by default; stdio servers only on request.** Probing a stdio server means starting its process, which can have side effects (for example codebase-memory starts its daemon).
   `llmenv doctor` probes `Remote` servers with transport `Http` every time.
   It probes `Stdio` servers only with a new flag, `llmenv doctor --probe-mcp`.
   `Sse` servers are not probed; print one `{info}` line saying so.
3. **Read-only.** The probe calls only `initialize` and `tools/list`. It never calls a tool.
4. **A failed probe is information, not a warning.** A server can be down while doctor runs.

## Design

### Limit

```rust
/// Claude Code's default cut for MCP tool descriptions and server instructions.
const CLAUDE_MCP_TEXT_LIMIT: usize = 2048;
```

Effective limit: the value of `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` from the process environment, else from `native.claude_code.env`, when it is 1 to 9 ASCII digits and not zero; otherwise `CLAUDE_MCP_TEXT_LIMIT`.
Use doctor's existing `effective_token_efficiency_var` helper to read it.

### Probe

New module `src/mcp/probe.rs`:

```rust
pub(crate) struct McpTextReport {
    pub server: String,
    pub instructions_chars: Option<usize>,
    /// (tool name, description chars), in the server's order.
    pub tools: Vec<(String, usize)>,
}

pub(crate) async fn probe(mcp: &ResolvedMcp, timeout: Duration) -> anyhow::Result<McpTextReport>;
```

- `ResolvedKind::Remote { transport: McpTransport::Http, url }`: build `McpHttpClient` with the same SSRF rules as `McpHttpClient::new`, plus the entry's `headers` as default request headers (add a constructor `McpHttpClient::with_headers(url, timeout, headers)` in the #2160 client for this). Call `server_instructions()` and `list_tools()`.
- `ResolvedKind::Stdio { command, args, env }`: use `rmcp`'s `TokioChildProcess` transport (feature `transport-child-process`, added by #2160) with `command`, `args` and `env`. Call `serve_client`, read `peer_info()` for instructions, call `list_all_tools()`, then cancel the service so the child exits. Kill the child if it has not exited 2 seconds after cancel.
- Other kinds: return `Err` with `transport <kind> is not probed`.
- A tool with no description counts as 0.
- Timeout per server: 5 seconds, covering connect and both calls.

### Doctor output

Add `probe_mcp: bool` to `Command::Doctor` (`#[arg(long)]`, help `Also start stdio MCP servers to measure their instructions and tool descriptions`) and pass it to `run_doctor` (4 positional parameters after the change, inside the limit of 5).

After the retired-settings section (#2145), when the Claude Code adapter is installed and the manifest has MCP servers, print `MCP text limits (Claude Code keeps <limit> characters):`, then one block per server in manifest order:

- Probe failed: `{info} <server>: not measured (<error>)`.
- Not probed (stdio without `--probe-mcp`): `{info} <server>: stdio server not started; run llmenv doctor --probe-mcp to measure it`.
- Instructions over the limit: `{warn} <server>: instructions are <n> characters; Claude Code sends the first <limit>`.
- Each tool over the limit: `{warn} <server>/<tool>: description is <n> characters; Claude Code sends the first <limit>`.
- Nothing over the limit: `{pass} <server>: instructions <n or "none">, <k> tools, longest description <m> characters`.

Run the probes concurrently with `tokio::task::JoinSet` (the crate has no `futures` dependency; do not add one), keep each result with its manifest index, then print in manifest order.

### Consolidation

In `call_claude` (`src/consolidation/mod.rs`), add `cmd.env("CLAUDE_CODE_MCP_STARTUP_WAIT_MS", "0");` before spawning.
Comment: the consolidation prompt calls no tools, so waiting for MCP servers only adds start-up time.
Older Claude Code versions ignore the variable.

## Tests

1. Limit parsing: unset, `"4096"`, `"0"`, `"abc"`, `"4096 "`, `"99999999999"` (more than 9 digits).
2. Character counting: a 2,048-character string of multi-byte characters is not over the limit; 2,049 is.
3. HTTP probe against a `wiremock` server returning instructions and two pages of tools.
4. Split the probe into transport setup and `async fn measure(service: &RunningClient) -> McpTextReport`.
   Test `measure` with an in-memory pair from `tokio::io::duplex`: the client side uses rmcp's async read/write transport, and the server side is a small `rmcp` server that returns fixed instructions and tools.
   Add `rmcp` to `[dev-dependencies]` with the same version and the extra features `server` and `transport-io` for this test only.
   The child-process path is covered by acceptance criterion 2, not by a unit test.
5. Doctor output formatting from a synthetic `McpTextReport` list (pure function; no network).
6. `call_claude` command test: the built command has `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` in its environment (extract command building into a function if it is not one already).

## Acceptance criteria

1. `llmenv doctor` with an ICM HTTP server configured prints a line for it with the instruction and tool counts.
2. `llmenv doctor --probe-mcp` also measures codebase-memory, and no `codebase-memory-mcp` child process is left running afterwards.
3. Changelog `Added`: doctor reports MCP text that Claude Code cuts. `Changed`: consolidation's `claude -p` call no longer waits for MCP servers.
4. `website/docs/` doctor page documents the check and `--probe-mcp`, tagged `(added in v3.12.0)`.

## Out of scope

- Shortening any server's text. The fix belongs to the server's owner.
- Probing `Sse` servers.
