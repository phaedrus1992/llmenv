# llmenv MCP layer: a small tool set in front of ICM

Status: draft for owner review, 2026-11-03.
Tracks issue #3104.
Related: #3105 (call ICM directly on its host), #2356 (`alwaysLoad` for ICM), #2497 (mods).
This document is planning only.
Nothing in it is built.

## Why

Every Claude Code session loads the full ICM tool surface up front.
That is 32 `mcp__icm__*` tools, about 16.5k characters of schema, and the names expose ICM internals.
On a local model, this is a real share of the prefill on every turn.

The model uses very little of it.
A scan of local session files from the last 14 days, across all projects, found 268 distinct ICM tool calls in 77 sessions.
228 were store, 17 were update, and 16 were recall.
Every other tool had one or two calls.
Hook-side calls are not in those files, so the scan covers the model only.

A second problem is the runtime path.
llmenv reaches ICM through `mcp-proxy`, a Python program, in front of a stdio `icm serve`.
That adds a Python dependency, and `mcp-proxy` needs a pinned `mcp` version to start.

A third problem is what gets stored and injected.
Stored memories are mostly long, dated work logs.
Decisions, lessons, and rules sit in the same record as the log.
By a rough keyword pass, about a third of the records mix two or more kinds.
The recall that hooks inject on each prompt is mostly these long logs.
Logs help mining later.
They do not help injection.

## What exists today

- `resolve_memory` in `src/mcp/resolve.rs` gives every agent an HTTP client at the memory server host.
  The local proxy in `src/mcp/proxy.rs` bridges the stdio `icm serve` process to that address.
  `always_load` defaults to true for ICM (#2356).
- llmenv hooks call ICM through `McpHttpClient` (`src/hook_run/mcp_client.rs`).
  The tools they call are `icm_wake_up`, `icm_memory_recall`, `icm_memory_store`, `icm_memory_forget`, the `icm_transcript_*` family, and `icm_memory_stats`, `icm_memory_list_topics`, and `icm_memory_health` for doctor.
- Permission tiers for the ICM tool names are in `src/adapter/mcp_tiers.rs` (`ICM_READ_ONLY`, `ICM_MUTATION`, `ICM_DESTRUCTIVE`).
  The Claude Code adapter prefixes them with `mcp__icm__` (`src/adapter/claude_code/mod.rs`).
- ICM 0.11.5 can serve MCP over HTTP itself.
  `icm serve --http <ADDR>` serves seven routes.
  Six are REST: `/recall`, `/store`, `/consolidate`, `/stats`, `/topics`, `/health`.
  The seventh, `POST /mcp`, carries JSON-RPC and reaches all the MCP tools.
  The `--help` text does not list `/mcp`.
  `/mcp` takes one POST per message and replies with JSON, or 204 for a notification.
  There is no GET or SSE stream.
  `--token` protects every route except `/health`.
  A bind to a non-loopback address is refused without a token.
  Source: `icm/crates/icm-cli/src/http_api.rs`, tag `icm-v0.11.5`.
- ICM's `POST /store` takes `topic`, `content`, `importance`, `keywords`, `raw`, and `created_at`.
  It has no tags, no project field, and no free metadata.
  `keywords` is the only per-memory label.
  `POST /recall` filters by `topic`, one `keyword` (a case-sensitive substring), and `project`.
  It has no tag filter and no date filter.
  Source: `http_api.rs` and `icm-core/src/lib.rs`, tag `icm-v0.11.5`.

## Overlap with existing issues

- #2160 replaces the hand-rolled MCP HTTP client with `rmcp`, the official Rust MCP SDK.
  Spec: `docs/design/issue-2160-rmcp-client.md`.
- #2161 builds an `rmcp` bridge inside llmenv that sits in front of a stdio `icm serve`, so `mcp-proxy` goes away.
  Spec: `docs/design/issue-2161-rmcp-icm-bridge.md`.
  Both issues are on v3.15.0.
- This design reaches the same goal as #2161 with less code, if ICM's own `/mcp` route works with Claude Code.
  Phase 1 tests that.
  If it works, #2161 is superseded.
  If it does not, #2161 is the fallback, and the rest of this design does not change.
- The new stdio server and the internal ICM adapter use `rmcp`, as #2160 directs.
  The project rule is to prefer an official SDK to a hand-rolled client.

## Goals

1. The model sees a small tool set that llmenv owns.
   Each tool has a short description written for the model, so it is cheap to load up front.
2. The tool names describe what llmenv does, not how ICM is built.
   A change of ICM version, or of the store, does not change what the model sees.
3. A stored memory carries a `kind` and tags, so injection and mining can select records.
4. Logs stay out of injection by default.
   They remain searchable through the same tool set.
5. Hooks and the model reach ICM through one path.
6. The Python `mcp-proxy` leaves the ICM path.
7. Admin and bulk operations stay out of the model's context.
   They remain on the CLI or in skills.

## Non-goals

- Replacing ICM as the store.
- Putting code search or the task tracker behind the layer now.
  The design leaves room for them.
- Changing ICM's code in this work.
  A change in the `phaedrus1992/icm` fork is a possible follow-up.
- Re-tagging existing memories.
  A memory with no kind reads as unknown kind and keeps working.
- Exposing memoirs, feedback, or embedding tools to the model.

## Anti-goals

These make the work a failure even if it runs.

- A per-call latency that a user can notice, compared with direct ICM.
- The model loses something it does today: store, update, or recall.
- A new always-on service that the user must watch.
- A memory write that fails with no notice to the agent.
  The project rule is that an error the agent needs reaches the agent.

## Decisions by the owner

- The model-facing layer is a "meta" layer.
  Its tools name the actions llmenv takes, and the backend routes them.
- Memories get kinds, such as logs, decisions, design information, and errors.
  The owner does not want to overdo the kind list.
- The agent can supply tags and other metadata when it stores, so llmenv can make better injection choices later.
- Logs are not injected by default, because they matter more for later mining.
- Basic querying is available through the layer.
- The work lands on `release/3.x`, and #3104 moves to milestone v3.15.0 next to #2160 and #2161.
  The forward-merge carries it to 4.x and main.
- A memory with no kind keeps being injected as today.
  Only memories tagged `kind:log` are skipped.
  A backfill of old records is a possible later task, and it is not part of this work.
- The owner leaves the build details to the builder.
  They are listed below as calls, and the owner can override each one.

## Calls made without the owner (override any of them)

1. **Shape.**
   llmenv ships the layer as a stdio MCP server inside its own binary, in Rust.
   The engine starts one per session.
   It forwards to a single shared ICM.
2. **Link to ICM.**
   The layer and the hooks call a running `icm serve --http` with a token, over `POST /mcp`.
   REST routes are an option for store and recall where they are simpler.
   llmenv manages that process in place of `mcp-proxy`.
3. **Kinds.**
   `log`, `decision`, `design`, `lesson`, and `rule`.
   No kind means unknown.
4. **Tag storage.**
   Kinds and tags ride in ICM's `keywords` as namespaced words, such as `kind:decision` and `tag:forward-merge`.
   This needs no ICM change.
5. **Tools.**
   - `remember`: content, kind, topic, tags, importance.
   - `recall`: a relevance query with optional kind, tag, and project filters.
     It leaves out `log` records unless the caller asks for that kind.
   - `list`: a filter-only listing by kind, tag, topic, or project, newest first, in pages.
   - `update` and `forget`.
   The set stays at about five tools.
6. **Hooks.**
   Hooks call the same operations through an internal Rust interface.
   One ICM adapter implements it.
7. **Permissions.**
   `recall` and `list` are read-only, `remember` and `update` are mutation, and `forget` is destructive.
   The tier tables get entries for the new names.
8. **Engines.**
   The layer is a resolved MCP entry like the others, so Claude Code, opencode, and Crush get it from the same source.
   The first engine to verify is Claude Code.
9. **Injection.**
   Once kinds exist, hooks inject decisions, lessons, rules, and design notes by relevance, and skip logs.
10. **Transport and one adapter.**
    One internal ICM adapter holds the transport.
    It uses MCP over HTTP through `rmcp` first, and REST for store and recall where that is simpler.
    The stdio server for the model links the same adapter in the same binary.
    It does not talk to the ICM daemon by a second path.
    A remote host keeps its current endpoint.
11. **Who runs the ICM daemon.**
    The host that owns ICM runs `icm serve --http`, under the same rules as today's proxy: the same host-ownership check, lock file, pidfile, and bound log.
    This replaces the proxy, so it is not a new service.
    llmenv generates a token on first start and stores it owner-only in the state directory.
    llmenv sends it as a bearer header.
    How a remote host receives the token is left to the builder.
    The token must not go into the config repository in plain text.
12. **The raw ICM tools.**
    Phase 3 removes the raw `mcp__icm__*` entry from the model's MCP config only.
    It is a config toggle.
    Hooks and doctor keep the full surface through the adapter.
    Skills and docs that name `icm_*` tools need an audit as a task in that phase.
13. **IDs and update.**
    `remember` returns the memory id.
    `update` and `forget` take an id.
    `update` replaces the content, kind, or tags only when the caller supplies them.
14. **Duplicate stores.**
    The layer keeps no state to catch them.
    It relies on the Claude Code version floor that doctor reports (#2551).
15. **`list` in the first version.**
    An approximation with recall is acceptable.
    Exact newest-first paging is not required, and a change to the ICM fork is not a prerequisite.
    The limits are documented.
16. **Latency target.**
    A warm call adds under 20 ms over a direct call to ICM, and session start adds under 150 ms.
    These are guesses.
    Measurement replaces them, and the owner sets the final threshold.

## Phases

1. **Drop the Python proxy.**
   Run `icm serve --http` with a token, in place of the proxy.
   No change for the model.
   Check first that Claude Code accepts `/mcp` with no GET stream.
   If it does, #2161 is superseded.
   If it does not, build the #2161 bridge, or keep the proxy until that ships.
   The later phases do not depend on which one wins, because the adapter hides the transport.
2. **One path for llmenv code.**
   Add the internal interface and move the hook calls onto it.
3. **The model-facing server.**
   Add the small tool set.
   Stop loading the raw `mcp__icm__*` tools for the model, and keep admin on the CLI.
4. **Kind-aware injection.**
   Change what hooks inject.
   Tell the model, in its tool descriptions, which kind to use.

Each phase can ship alone.

## Open risks for the builder

- `/mcp` compatibility.
  Claude Code may expect a stream on a GET.
  Test this before any other work.
- `list` has no direct ICM call.
  ICM's recall needs a query, and `icm_memory_list_topics` returns topics only.
  The layer may need to approximate a listing with recall, or ICM may need a change in the fork.
  Confirm against a live server.
- A multi-tag filter.
  ICM filters by one keyword substring.
  The layer can fetch extra results and filter them itself.
  Measure the cost.
- Latency.
  The layer adds a process start and one hop.
  Measure it against direct ICM.
- Duplicate stores.
  Claude Code before 2.1.307 sent some HTTP MCP calls twice.
  ICM does not mark store as idempotent (`phaedrus1992/icm#5`).
- Process ownership.
  Decide who starts the ICM HTTP daemon on the host, and where the token lives.
- Existing users.
  Permission rules and docs that name `mcp__icm__*` tools need a migration note.
- Forward-merge.
  The 4.x line has a separate `llmenv-mcp` crate.
  Expect conflicts when the work moves from `release/3.x`, as earlier sprints did.

## Left to the builder

- Final names for the tools and the launch command.
- The exact tag word format and the allowed characters.
- The wording of the tool descriptions.
- Paging and limit defaults for `list` and `recall`.
- Test design, and how to measure latency.

## Done means

- A session shows only the llmenv tool set for memory, and the schema size is a fraction of today's.
- The model can store, recall, list, update, and forget through it.
- A log stored with kind `log` is findable through `recall` with the kind filter, and through `list` within the stated limits.
  No hook injects it.
- The Python `mcp-proxy` is not needed on the ICM path.
- A failed store or recall reaches the agent as a notice with the cause and the fix.
- `llmenv doctor` checks the new path.
