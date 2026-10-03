# Issue #2356 — render `alwaysLoad` for MCP servers, on by default for ICM

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2356
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (config field, resolve, Claude Code render)

This is a spec, not a plan.

## Problem

Claude Code defers MCP tools behind tool search.
Every `icm_*` tool arrives deferred, so the model must call `ToolSearch` before it can recall or store anything.
That is friction on the tools our instructions tell it to use aggressively (a live 2.1.287 session showed all 31 ICM tools deferred).
Claude Code has a per-server `alwaysLoad` key for this, and llmenv cannot render it except through a hand-written `native_mcp.claude_code` overlay.

## Claude Code facts (changelog and MCP docs, fetched 2026-10-02)

| Version | Fact |
| --- | --- |
| 2.1.121 | `alwaysLoad` added to MCP server config: `true` means all tools from that server skip tool-search deferral |
| 2.1.285 | a tool whose own `_meta['anthropic/alwaysLoad']` is `false` stays deferred even when its server is `alwaysLoad` |
| 2.1.287 | `alwaysLoad: false` defers all of that server's tools |
| MCP docs | the `ws` server type "accepts the same `url`, `headers`, `headersHelper`, `timeout`, and `alwaysLoad` fields as `http`", so remote entries take it; the 2.1.288 binary has 54 hits for the key |

The settings reference does not list the key; the MCP page and the changelog are the sources.
Verify on a live session that a stdio entry with `alwaysLoad: true` is honored too (the ICM server is remote `http`, so the stdio case matters only for user entries); record the result in this table.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `McpServer { name, when, transport (serde "type"), command, args, env, url, headers, disabled, disabled_tools, timeout: Option<u32> }`, not `deny_unknown_fields` | `crates/llmenv-config/src/schema.rs` |
| `ResolvedMcp { name, kind: Stdio | Remote, headers, timeout, disabled_tools, mcp_permissions, memory_hook }`; set in `resolve_static` (top-level and bundle `mcp`, both transports), `resolve_memory` (the built-in ICM entry, remote http, `timeout: None`), `resolve_codebase_memory` | `src/mcp/resolve.rs` |
| `build_mcp_servers` renders stdio entries as `command`/`args`/`env` and remote entries as `type`/`url`/`headers`, with `timeout` only when `Some` and only on remote entries | `src/adapter/claude_code/mod.rs` |
| `RENDERED_ENTRY_KEYS` must list every key `build_mcp_servers` writes; the test `rendered_entry_keys_cover_every_key_build_mcp_servers_writes` enforces it; `carry_runtime_keys` keeps Claude's own runtime keys across re-renders (#2376) | `src/adapter/claude_code/mod.rs` |
| `overlay_native` applies `native_mcp.claude_code.mcpServers.<name>.*`, today's workaround | `src/adapter/claude_code/mod.rs` |
| Crush builds its map by hand (`timeout` on both transports); opencode uses the typed `McpEntry` with `JsonSchema`; both copy named fields, so an unread field is skipped | `src/adapter/crush.rs`, `src/adapter/opencode.rs` |
| `features.memory[]` entries already carry per-feature switches such as `adaptive_recall` | `crates/llmenv-config/src/schema.rs` |
| Tests to copy: `headers_and_timeout_flow_through_resolution`; `merge_mcp_drops_stale_config_keys_but_keeps_runtime_keys`; the `stdio_mcp` / `remote_mcp` helpers | `src/mcp/resolve.rs`, `src/adapter/claude_code/mod.rs` |
| Docs: the `mcp:` field table in `website/docs/configuration.md` (no `timeout`, `headers`, or `disabled_tools` rows today) | docs |

## Decisions

1. **`always_load: Option<bool>` on `McpServer` and on `ResolvedMcp`.**
   `None` renders nothing, so Claude Code's default applies and existing configs render byte-for-byte the same.
2. **Rendered for both transports** in the Claude Code adapter when `Some`.
   Crush and opencode ignore it; their builders are not touched.
3. **The built-in ICM entry defaults to `Some(true)`.**
   `features.memory[]` gets `always_load: Option<bool>`; `resolve_memory` passes `Some(cfg.always_load.unwrap_or(true))`.
   A user who wants the old behavior sets `features.memory[].always_load: false`.
4. **codebase-memory stays `None`.**
   Its tools are used on demand and tool search fits them.
   A user can set it per entry if `CodebaseMemory` gains the field later; not in this issue.
5. **`alwaysLoad` joins `RENDERED_ENTRY_KEYS`**, so a value removed from config is dropped on re-render (#2376 rule), and the coverage test passes.
6. **The configuration docs gain the missing rows** (`timeout`, `headers`, `disabled_tools`, `always_load`) while the table is open; those fields exist and are undocumented, which is a docs bug this change would otherwise walk past.

## Design

### Config (`crates/llmenv-config/src/schema.rs`)

On `McpServer`:

```rust
/// Claude Code `alwaysLoad`: `true` keeps every tool of this server in the
/// prompt instead of behind tool search; `false` defers them all. Unset
/// leaves Claude Code's default. Ignored by engines without the key.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub always_load: Option<bool>,
```

On the memory feature entry: the same field, documented as "defaults to `true` for the ICM server".

No validation needed; the type is the validation.

### Resolve (`src/mcp/resolve.rs`)

- `ResolvedMcp.always_load: Option<bool>`.
- `resolve_static` copies it from the entry.
- `resolve_memory` applies the `true` default.
- `resolve_codebase_memory` sets `None`.
- `mcp_health` fixtures and any struct literal gain the field (the compiler finds them).

### Render (`src/adapter/claude_code/mod.rs`)

- In `build_mcp_servers`, after the transport-specific keys: `if let Some(v) = mcp.always_load { entry.insert("alwaysLoad", v) }`, for both branches.
- Add `"alwaysLoad"` to `RENDERED_ENTRY_KEYS`.

### Docs

- `website/docs/configuration.md`, `## mcp:` table: `always_load` `(added in v3.12.0)`, plus the three missing existing rows (look up their introducing versions in the changelog for their tags).
- `### features.memory:`: `always_load`, default `true`, `(added in v3.12.0)`, with one sentence on why (ICM tools are used on every prompt).
- `website/docs/mcp.md` memory section: ICM tools are no longer deferred by default; how to turn that off.
- Changelog `Added`: `mcp[].always_load` and `features.memory[].always_load`; `Changed`: ICM tools load without a tool-search round trip by default.

## Tests

1. Resolve: a stdio and a remote `McpServer` with `always_load: Some(false)` resolve with the value; unset resolves `None`.
2. `resolve_memory`: no setting → `Some(true)`; `always_load: false` → `Some(false)`.
3. `resolve_codebase_memory` → `None`.
4. Render: `Some(true)` on stdio and on remote each produce `"alwaysLoad": true`; `None` produces no key; `Some(false)` produces `false`.
5. `rendered_entry_keys_cover_every_key_build_mcp_servers_writes` passes with the new key.
6. Re-render: an entry that had `alwaysLoad` and loses it in config has no `alwaysLoad` after `merge_mcp_into_claude_json`.
7. Crush and opencode: an `McpServer` with `always_load: Some(true)` renders identically to one without (snapshot or map equality).
8. Property test: for any `McpServer`, the rendered Claude entry has `alwaysLoad` if and only if `always_load.is_some()`, and the value matches.

## Acceptance criteria

1. A new Claude Code session with ICM configured lists `icm_*` tools as regular tools, not deferred (check the system prompt's tool list or call one without `ToolSearch`).
2. `features.memory[].always_load: false` restores deferral.
3. A user `mcp:` entry with `always_load: true` renders the key.
4. Changelog entries; configuration and MCP docs updated with the version tags.

## Out of scope

- Per-tool `_meta['anthropic/alwaysLoad']` (that is the server's call).
- `always_load` on `CodebaseMemory`.
- `ENABLE_TOOL_SEARCH` or other tool-search environment variables.
