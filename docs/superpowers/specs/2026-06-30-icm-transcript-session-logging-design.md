# ICM-transcript session logging — design

- **Issue:** #382 (reopened, redirected from file-only JSONL to ICM transcripts)
- **Date:** 2026-06-30
- **Status:** design, pending implementation plan

## Problem

llmenv has no proper session logging. The only piece that shipped is the
`session_log = "<path>"` string field (2.1.0), which wires a
`tracing-subscriber` file layer that dumps llmenv's **internal `tracing`
events** as JSONL (`src/main.rs:4-30`). That diagnoses llmenv's own hooks /
materialization, but it does **not** document the agent session itself — the
prompts, the tool calls, the scope (tags/bundles) that was active.

We want session activity recorded into **ICM's transcript store** so it is
durable, queryable, and discoverable by the llmenv scope that produced it.

## What ICM gives us (from the reference, `~/git/reference/icm`)

llmenv talks to ICM **only through the MCP** (see "Transport" for why — multi-host).
The transcript MCP tools we call:

- `icm_transcript_start_session(agent, project, metadata)` → returns a
  `session_id`.
- `icm_transcript_record(session_id, role, content, tool_name?, tokens?,
  metadata?)`.
- (read side, for users) `icm_transcript_search(query, project?, session_id?,
  limit?)`, `icm_transcript_show(session_id)`.

These are the same operations the `icm transcript …` CLI exposes, but the CLI
writes to a **local** sqlite store — wrong when the caller isn't the primary
ICM host. The MCP endpoint (which may be a remote `icm serve` on the primary
host) is the only correct runtime path.

Store shape (`crates/icm-store/src/store.rs`):

- `sessions(id, agent, project, started_at, updated_at, metadata)`
- `messages(id, session_id, role, content, tool_name, tokens, ts, metadata)`

**Hard constraint that drives the design:** transcript *search*
(`search_transcripts`) is FTS5 (`messages_fts MATCH`) over message **content
only**. Session `metadata` and `project` are returned and `project`/`session_id`
can *filter*, but `metadata` is **not searchable**. So anything we want to
*find a transcript by* (tags, bundles) must appear in message **content**, not
only in metadata.

Note: ICM's `icm hook` commands (pre/post/start/end) only **extract memories**
from Claude's transcript file; they do **not** populate the transcript store.
There is no "let ICM ingest it" shortcut — to get agent turns into the
transcript store, llmenv must record them explicitly.

## Model: one stream, two sinks

Session logging is a single stream of `SessionLogEvent`s. Two **fully
independent** sinks consume the **same** events:

1. **file** — append JSONL to a file (durable, grep-able, local; never depends
   on ICM).
2. **transcript** — `icm_transcript_record` via the ICM MCP (queryable,
   cross-session; degrades to no-op when the MCP is unreachable).

`verbose` controls *how much* enters the stream, not which sink receives it.
Both sinks always get identical events; either can be on without the other.

### Event sources

The stream is fed from three places, all converging on the same sinks:

- **Lifecycle/scope** (in-process): explicit `lifecycle_start` / `scope` /
  `lifecycle_end` emits at launch and teardown.
- **Internal operations** (in-process): a `tracing_subscriber::Layer` that
  maps llmenv's own `tracing` events at `info`+ into `internal` events —
  materialization, change detection, cache sync, regenerate, auth detection,
  hook firing, etc. This **keeps the useful internal logging** that 2.1.0's
  file did, now flowing into both sinks. `level` is carried as a field.
- **Agent turns** (out-of-process, verbose only): injected Claude hooks →
  `llmenv hook-run …` → `prompt` / `tool_use` / `tool_result` / `stop` events.

### `SessionLogEvent`

```
ts:        RFC 3339
kind:      lifecycle_start | scope | internal | prompt | tool_use
           | tool_result | notification | stop | lifecycle_end
role:      user | assistant | system | tool     # for the transcript mapping
tool_name: Option<String>
tokens:    Option<u64>
level:     Option<String>                        # for internal events (info/warn/…)
content:   String                                # rendered, FTS-searchable
fields:    structured payload (tags, bundles, scopes, cwd, op name, …)
```

- **File sink:** serialize the event to one JSON line, append.
- **Transcript sink:** `icm_transcript_record(session_id, role, content,
  tool_name?, tokens?, metadata=fields)` via the MCP.

## Layers

### Baseline (feature enabled, `verbose = false`)

1. At launch / `llmenv export`: call `icm_transcript_start_session(agent=<adapter>,
   project=<scope-project-or-cwd-basename>,
   metadata={tags, bundles, scopes, cwd, adapter, llmenv_version})` via the MCP.
   Persist the returned `session_id` (see State). When `file` is on, the same
   scope/lifecycle events are written to the file regardless of MCP state.
2. Emit a **scope-header** event (`kind=scope`, `role=system`) whose `content`
   embeds discoverability tokens reusing the existing convention in
   `src/icm.rs`:
   `llmenv-tag:<tag>` (one per active tag) and `llmenv-bundle:<bundle>`
   (one per bundle), plus the project name. This is the FTS handle.
3. Inject **minimal lifecycle hooks** — `SessionStart` and `SessionEnd`
   (fall back to `Stop` if the adapter lacks `SessionEnd`) — so the recorded
   session brackets the *real* agent session, not just the `export` call.
   `SessionStart` emits `lifecycle_start`; `SessionEnd`/`Stop` emits
   `lifecycle_end`.

### Verbose (`verbose = true`)

In addition to the baseline, inject llmenv handlers on **all** remaining Claude
Code hook events and record each as an event:

| Hook event       | event kind     | role  |
|------------------|----------------|-------|
| UserPromptSubmit | prompt         | user  |
| PreToolUse       | tool_use       | tool  |
| PostToolUse      | tool_result    | tool  |
| Notification     | notification   | system|
| Stop             | stop           | assistant |
| SubagentStop     | stop           | assistant |
| PreCompact       | notification   | system|

`tool_name` is taken from the hook payload for tool events. Content is the
prompt text / tool input / tool result, **truncated to `max_content_bytes`**
(default 16 KiB) to bound DB growth and hook latency.

## Transport & wiring

- **All runtime ICM interaction goes through the MCP, never the `icm` CLI.**
  Session logging may be collected on a machine that is **not** the primary ICM
  host; the `icm` CLI would write to that machine's *local* sqlite store, which
  is the wrong store. The MCP endpoint llmenv already resolves
  (`src/mcp/resolve.rs`, possibly a remote http `icm serve` on the primary host)
  is the single correct target. This rule is added to `AGENTS.md`.
- The transcript sink uses a **minimal MCP client** (JSON-RPC `tools/call`) that
  issues `icm_transcript_start_session` / `icm_transcript_record` against the
  resolved `icm` server endpoint (stdio or http). No direct `icm-store` sqlite
  coupling; no CLI shell-out.
- **Timing — MCP calls are dispatched off the critical path.** An MCP round trip
  (especially to a remote host) must not delay `llmenv` returning or a Claude
  hook completing (Claude kills slow hooks). The transcript sink enqueues records
  to a background worker (thread, or detached subprocess for hook processes that
  exit immediately — the ICM `cmd_hook_end` `setsid` detach is the template) and
  returns immediately. The **file** sink stays synchronous (a local append is
  cheap and must not be lost).
- Reuse the existing hook-injection machinery in
  `src/adapter/claude_code.rs` and the `llmenv hook-run` dispatcher
  (`src/hook_run/mod.rs`, `HookEvent` enum + `run(event)`): add the new events
  and a session-log action. Injected hook commands call `llmenv hook-run <event>`
  and read the hook payload (incl. Claude `session_id`) from stdin, exactly like
  today's hooks. The `hook-run` process resolves the same MCP endpoint + session
  map and dispatches the record in the background before exiting.
- New core module `src/session_log/` (emitter + the two sinks + event model +
  the `tracing` Layer for internal events). `src/icm.rs` keeps its current
  tag/bundle context-chunk role; the `llmenv-tag:` / `llmenv-bundle:` token
  format is shared between them.

## State & correlation

- llmenv owns **one transcript session per agent launch**.
- Claude hooks deliver Claude's `session_id` on stdin. llmenv keeps a
  `claude_session_id → {icm_session_id, file_path}` map under `state_dir()`
  (`~/.local/state/llmenv`, or `$LLMENV_STATE_DIR`) — a **stable state path,
  not cache** (per the #382 follow-up note). Written owner-only (0o600) via the
  existing `write_owner_only_atomic` helper.
- First hook for an unseen Claude session lazily creates the icm session if the
  launch-time start was skipped (e.g. icm was briefly unavailable).

## Configuration

llmenv config is **YAML**. This is a **3.0 major release**, so we take the
breaking change cleanly: the 2.1.0 `session_log: "<path>"` string form (raw
`tracing` JSONL dump) is **removed and replaced** by a mapping. No back-compat
shim.

```yaml
session_log:
  file: false        # write the session-event stream as JSONL (default off)
  transcript: true   # write the same stream to ICM transcripts (DEFAULT ON)
  verbose: false     # include per-hook prompt/tool detail (default off)
  # path: "..."             # override file-sink path (default <state_dir>/session-log.jsonl)
  # max_content_bytes: 16384
```

**Defaults — ICM transcript is on, verbose off.** With **no `session_log`
block at all**, the effective config is `{file: false, transcript: true,
verbose: false}`: every launch opens an ICM transcript session and records the
scope-header + lifecycle baseline. `Config.session_log` therefore uses a
`Default` impl returning `transcript = true` (not `Default::default()` zeros).

To disable entirely, set `transcript: false` (and `file: false`). When the ICM
MCP is unreachable, only the **transcript** sink no-ops; if `file: true` the
file sink still records everything (see Degradation).

`session_log` now parses **only** as a mapping. A bare-string value is a config
error (the validator reports the migration: use `session_log: { file: true }`).
The 2.1.0 `file` form dumped raw `tracing` lines; the new `file` sink emits the
richer **session-event stream** — which still includes the useful internal
operations (materialization, change detection, …) via the `tracing` Layer, plus
scope/lifecycle and (when verbose) agent turns. stderr `tracing` is unchanged.

## Examples

Update the in-repo example config to demonstrate the new setting (examples are
illustrative configuration, the correct place to show user-facing config —
per `AGENTS.md`):

- `examples/config-llmenv-dir/config.yaml`: add a documented `session_log:`
  block in the house style (heavy explanatory comments), showing the three
  flags and stating the default (transcript on, verbose off). Make explicit
  that omitting the block still yields ICM-only logging.
- Note: the unrelated `bundles/base/hooks/session-log.sh` example hook is a
  user-authored bundle hook, not this feature — leave it, but the new block's
  comment should avoid implying they are the same thing.

## Degradation & safety

- **Sinks are independent.** The MCP being unreachable, the endpoint
  unresolved, or any `start_session` / `record` failure affects **only** the
  transcript sink → logged at `debug!`, dropped. If `file: true`, the file sink
  records the full stream regardless. Session logging **never** fails a launch
  and **never** blocks on the network.
- **Background dispatch** for transcript records (thread / detached subprocess)
  so a slow or remote MCP never delays `llmenv` or a Claude hook. The file sink
  is a synchronous local append (cheap, must not be lost).
- Content size capped per event (`max_content_bytes`).
- Tokens like `llmenv-tag:<tag>` are validated with the existing `validate_tag`
  / `validate_bundle` guards before being written, preventing FTS/`content`
  injection.

## Discoverability / queryability (the explicit requirement)

Four handles, documented with recipes in user docs (via `icm_transcript_search`
MCP tool or the `icm transcript search` CLI when on the host):

- **Project filter:** search with `project = <name>`.
- **Agent filter:** session `agent` = adapter name (returned in results).
- **FTS tag/bundle tokens:** search `"llmenv-tag:rust"` → the scope-header
  message → its session.
- **Structured metadata:** full `{tags, bundles, scopes, cwd, adapter,
  llmenv_version}` JSON on the session for exact inspection/replay.

## Testing

- Config: an absent `session_log` block yields `{file: false, transcript:
  true, verbose: false}`; explicit mapping parses and round-trips; a
  bare-string `session_log` is rejected with the migration message.
- Scope-header token formatting — property test reusing the `llmenv-tag` /
  `llmenv-bundle` convention and the `validate_*` guards.
- `SessionLogEvent` → file JSONL line and → `icm_transcript_record` call args
  are consistent (same content/role).
- **Independence:** with `file: true` and the MCP unreachable, every event still
  lands in the file (the key guarantee); with `transcript: true` only, an MCP
  failure produces no file and no error.
- Internal-op `tracing` Layer: an `info`+ event (e.g. a materialization log) is
  captured as a `kind=internal` event with its `level`.
- `claude_session_id → icm_session_id` map: store/recall round-trip, 0o600
  perms (property test, mirrors the existing `icm.rs` perms test).
- Hook payload (stdin JSON) → event mapping for each of the verbose events.
- Content truncation at `max_content_bytes` boundary.
- Background dispatch does not block: a stubbed slow MCP transport still returns
  control promptly (assert the call path doesn't await the round trip).

## Milestone / branch

Target the **3.0 major release** off `main`. The breaking `session_log` config
change is the reason it belongs in a major, not a point release. #382 was filed
under `2.1 — Config & CLI DX` (minor); it is **re-scoped to a 3.0 major
feature** and the issue milestone should be updated to match.

## Out of scope (v1)

- Transcript rotation / retention (ICM owns its store lifecycle).
- Adapters other than Claude Code (design leaves room; only Claude Code injects
  hooks today).
- Recording into the agent's *own* ICM MCP session (llmenv owns a distinct
  session; not shared).
```
