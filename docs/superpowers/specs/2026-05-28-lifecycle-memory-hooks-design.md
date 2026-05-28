# Lifecycle Memory Hooks Design

**Status:** Draft → for implementation
**Date:** 2026-05-28
**Owner:** breed

## Summary

Give llmenv a way to inject memory context into an AI coding agent automatically,
as part of a scope's configuration, by talking to the configured ICM memory MCP
over HTTP. llmenv gains an engine-neutral lifecycle-hook dispatcher,
`llmenv hook-run <event>`, that the Claude Code adapter wires into the agent's
native hooks (settings.json). On `session_start` it injects an ICM wake-up pack;
on `turn_start` it injects recalled context; on `session_end` it does a
best-effort store of the active scope context.

This mirrors the *read* and *best-effort-store* halves of `icm hook ...`, but
reaches ICM through the MCP server llmenv already resolves rather than a local
`icm` CLI — so it works when ICM is remote-only.

## Motivation

llmenv already resolves an ICM memory backend (`memory:` config block →
`http://<host>:<port>` via `resolve_memory`) and emits it into the materialized
`mcp.json` as the `icm` MCP server. It also builds `LLMENV_ICM_CONTEXT` (active
tags/bundles/project) during `export`. What it does *not* do is act on that
context automatically: today the agent must proactively call ICM memory tools,
nudged only by the MCP server's instruction string.

`icm hook ...` solves this locally by wiring agent lifecycle hooks that call ICM.
But those hooks shell out to a local `icm` binary, which is unavailable in a
distributed setup where ICM runs on another host and is reachable only over MCP.

This feature lets llmenv own the lifecycle hooks and route them through the MCP
backend it already knows about, so context injection is automatic and deterministic
regardless of where ICM runs.

## Goals

- `llmenv hook-run <event>` dispatcher with three engine-neutral events:
  `session_start`, `turn_start`, `session_end`.
- Each event runs a memory action against the configured ICM memory MCP over HTTP:
  wake-up, recall, best-effort store (respectively).
- Adapter-routed context emission: the adapter formats injected context in the
  engine's native hook-output shape.
- Auto-wiring: when a scope's active tags select the `memory:` backend, the
  adapter emits these lifecycle hooks into the agent config (like `check-stale`
  is auto-emitted today).
- Fail-soft: a dead or unreachable MCP never blocks or meaningfully delays the
  agent; failures warn on stderr and exit 0.

## Non-Goals

- Transcript-extraction hooks (`icm hook post` / `compact` full fidelity). These
  read the local transcript file and spawn async extraction workers — there is no
  clean MCP equivalent, and reimplementing the extractor in llmenv is out of scope.
  Memory *extraction* remains the agent's job (nudged by the MCP server's own
  instructions). llmenv only does a lightweight best-effort store on `session_end`.
- A permission-intercept hook (`icm hook pre`). It auto-allows local `icm` CLI
  commands; in remote-only mode there are no local `icm` calls to allow, so it is
  moot.
- Changes to the existing `llmenv hook zsh|bash` shell-integration codegen. That
  is a different concept (shell precmd wiring) and is untouched.
- Folding the existing `check-stale` SessionStart hook into the new dispatcher.
  This is a natural future consolidation but is deliberately left for a follow-up
  to keep this change focused.
- Codex (or any non-Claude) adapter implementation. The design keeps the
  neutral→native translation in the adapter so a future Codex adapter can be added
  without touching the dispatcher, but no Codex adapter ships here.

## Background: hook vocabularies

Claude Code and Codex use nearly identical hook vocabularies and the *same*
context-injection JSON (`hookSpecificOutput.additionalContext`). The two intents
this feature most needs map 1:1 across both engines; the third has a gap on Codex:

| Neutral event  | Intent                              | Claude Code       | Codex            |
| -------------- | ----------------------------------- | ----------------- | ---------------- |
| `session_start`| inject context once at session begin| `SessionStart`    | `SessionStart`   |
| `turn_start`   | inject context per user prompt/turn | `UserPromptSubmit`| `UserPromptSubmit`|
| `session_end`  | run on session end (store)          | `SessionEnd`      | no equivalent¹   |

¹ Codex has no documented session-end hook; the closest is `Stop` (per-turn). A
future Codex adapter decides whether to map `session_end` to `Stop`, polyfill, or
omit. That decision lives in the adapter, not the dispatcher.

## Architecture

### Command surface

```
llmenv hook-run <event>      # event ∈ { session_start, turn_start, session_end }
```

`hook-run` is an **event dispatcher**: given an event, it runs the ordered list of
actions configured for that event. v1 mapping (one action per event):

| Event           | Action   | MCP tool             |
| --------------- | -------- | -------------------- |
| `session_start` | WakeUp   | `icm_wake_up`        |
| `turn_start`    | Recall   | `icm_memory_recall`  |
| `session_end`   | Store    | `icm_memory_store`   |

The dispatcher shape (`event → Vec<Action>`) lets a single event gain more actions
later (e.g. `session_start` could also run a drift check) without changing the
command surface.

### Module layout

New module `src/hook_run/` (sibling to `src/icm.rs`):

```
src/hook_run/
  mod.rs        # dispatch(event) -> ordered actions; run() entry called by CLI
  action.rs     # Action enum { WakeUp, Recall, Store } + per-action logic
  mcp_client.rs # minimal HTTP JSON-RPC MCP client (initialize + tools/call)
```

The CLI subcommand handler in `src/cli/mod.rs` parses `<event>` and calls
`hook_run::run(event)`.

### HTTP MCP client

A minimal client, not a general MCP library — only the calls this feature needs:

1. Resolve the memory backend via the existing `resolve_memory`, yielding
   `http://<host>:<port>` and the HTTP transport.
2. MCP `initialize` handshake over HTTP JSON-RPC.
3. `tools/call` for the one tool the action needs (`icm_wake_up`,
   `icm_memory_recall`, or `icm_memory_store`).
4. Parse the result text out of the tool response.

Built on serde_json and the HTTP dependency llmenv already uses. No new MCP-client
crate. All network calls are bounded by a tight timeout constant (2–3s).

### Action inputs

- **WakeUp** (`session_start`): no query; calls `icm_wake_up`, returns the pack.
- **Recall** (`turn_start`): builds a query from the same inputs that feed
  `LLMENV_ICM_CONTEXT` — active tags and project (reuse the data path behind
  `icm::generate_context_chunk`). Returns recalled context.
- **Store** (`session_end`): best-effort `icm_memory_store` of the llmenv context
  chunk (active tags/bundles/project), keyed under the existing
  `llmenv-tag:<tag>` keyword convention. No transcript reading, no LLM extraction.

### Context emission (adapter-routed)

The dispatcher returns plain text. Formatting for the engine belongs to the
adapter (which already owns settings.json emission). Extend the `AgentAdapter`
trait:

```rust
fn emit_hook_context(&self, text: &str) -> String;
```

- Claude Code impl: returns `{"hookSpecificOutput":{"additionalContext": text}}`
  (the shape shared by Claude Code and Codex). Empty `text` → empty string, which
  suppresses injection.
- `hook-run` selects the active adapter and prints `emit_hook_context(result)` to
  stdout.

### Auto-wiring (config gating)

No new config field. The `memory:` block already gates whether the `icm` MCP is
emitted. Reuse that signal: when a scope's active tags select the memory backend,
the Claude Code adapter auto-emits three lifecycle hooks into settings.json, each
running the corresponding `llmenv hook-run <native-event>` command — exactly the
pattern the auto-emitted `check-stale` SessionStart hook uses today. The adapter
performs the neutral→native event-name translation when it writes these entries.

If no memory backend is active for the scope, no lifecycle hooks are emitted.

### Failure mode

Fail-soft, with a tight timeout and a visible-but-nonblocking warning:

- Network call timeout, unreachable host, or MCP error → print one line to stderr
  (`llmenv: memory <event> skipped: <reason>`), emit empty stdout, exit 0.
- Rationale: lifecycle hooks run on the agent's hot path (startup, every prompt).
  A remote/down ICM must never block or delay the agent. This matches
  `check-stale`'s stance that drift is a warning, not an error. The stderr line
  keeps silent degradation visible to the user.

## Errors & diagnostics

- All hook failures degrade to exit 0 with a single stderr warning line including
  the event and the reason (timeout / connection refused / MCP error message).
- `doctor` is not extended in this change (memory-backend reachability is already
  surfaced there). A future follow-up could add a "lifecycle hooks active for
  scope X" line.

## Testing strategy

- **Dispatcher mapping** — table test: each event maps to the expected ordered
  action list.
- **HTTP MCP client** — against a mock HTTP server: `initialize` handshake,
  `tools/call` request shape per action, response-text parsing, timeout path,
  connection-error path.
- **Adapter emit** — `emit_hook_context` JSON shape snapshot; empty-text
  suppression returns empty string.
- **Adapter wiring** — settings.json contains the three lifecycle hooks iff the
  memory backend is active for the scope; asserted both ways (present when active,
  absent when not).
- **Fail-soft** — unreachable URL: process exits 0, prints the stderr warning,
  emits empty stdout (no partial/garbage JSON).

## Platform / runtime

No new dependencies beyond serde_json and the existing HTTP stack. Single binary,
no daemon. Same platform matrix as the rest of llmenv.

## Open questions (none blocking)

- Exact timeout constant (2s vs 3s) — pick during implementation; small.
- Whether `turn_start` recall should be capped in length before injection — start
  uncapped (the MCP returns a bounded pack) and revisit if prompts get noisy.
