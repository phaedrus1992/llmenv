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

CLI surface llmenv shells out to (mirrors the `icm_transcript_*` MCP tools):

- `icm transcript start-session --agent <a> --project <p> --metadata <json>`
  → prints a `session_id`.
- `icm transcript record --session <id> --role <user|assistant|system|tool>
  --content <text> [--tool-name <n>] [--tokens <n>] [--metadata <json>]`.

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

Session logging is a single stream of `SessionLogEvent`s. Two independent sinks
consume the **same** events:

1. **file** — append JSONL to a file (the durable, grep-able local copy).
2. **transcript** — `icm transcript record` into ICM's store (queryable,
   cross-session).

`verbose` controls *how much* enters the stream, not which sink receives it.
Both sinks always get identical events.

### `SessionLogEvent`

```
ts:        RFC 3339
kind:      lifecycle_start | scope | prompt | tool_use | tool_result
           | notification | stop | lifecycle_end
role:      user | assistant | system | tool     # for the transcript mapping
tool_name: Option<String>
tokens:    Option<u64>
content:   String                                # rendered, FTS-searchable
fields:    structured payload (tags, bundles, scopes, cwd, …) for the scope kind
```

- **File sink:** serialize the event to one JSON line.
- **Transcript sink:** `icm transcript record --session <id> --role <role>
  --content <content> [--tool-name ...] [--tokens ...] --metadata <fields-json>`.

## Layers

### Baseline (feature enabled, `verbose = false`)

1. At launch / `llmenv export`: `icm transcript start-session
   --agent <adapter> --project <scope-project-or-cwd-basename>
   --metadata <{tags, bundles, scopes, cwd, adapter, llmenv_version}>`.
   Persist the returned `session_id` (see State).
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

- llmenv core shells out to the `icm` CLI (matches the existing `icm serve`
  launch). No direct `icm-store` sqlite coupling (version-fragile across the
  external dependency); no MCP-from-core.
- Reuse the existing hook-injection machinery in
  `src/adapter/claude_code.rs` and the `llmenv hook-run` dispatcher
  (`src/hook_run/mod.rs`, `HookEvent` enum + `run(event)`): add the new events
  and a transcript-record action. Injected hook commands call
  `llmenv hook-run <event>` and read the hook payload (incl. Claude
  `session_id`) from stdin, exactly like today's hooks.
- New core module `src/session_log/` (emitter + the two sinks + event model).
  `src/icm.rs` keeps its current tag/bundle context-chunk role; the
  `llmenv-tag:` / `llmenv-bundle:` token format is shared between them.

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

To disable entirely, set `transcript: false` (and `file: false`). When `icm`
is unavailable the baseline degrades to a no-op regardless (see Degradation).

`session_log` now parses **only** as a mapping. A bare-string value is a config
error (the validator reports the migration: use `session_log: { file: true }`).
The old "raw llmenv tracing → file" behavior is gone; internal `tracing`
diagnostics remain available on stderr as before. The `file` sink now emits the
**session-event stream** (the same events the transcript sink gets).

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

- `icm` not on `PATH`, or any `start-session` / `record` failure → log at
  `debug!` and continue. Session logging **never** fails a launch.
- Hook handlers must return fast (Claude kills slow hooks). A `record` is a
  single sqlite insert via the CLI — fast enough synchronously. If it proves
  slow in practice, move the transcript sink to fire-and-forget (the ICM
  `cmd_hook_end` detach pattern is the template). Start synchronous (YAGNI).
- Content size capped per event (`max_content_bytes`).
- Token tokens like `llmenv-tag:<tag>` are validated with the existing
  `validate_tag` / `validate_bundle` guards before being written, preventing
  FTS/`content` injection.

## Discoverability / queryability (the explicit requirement)

Four handles, documented with recipes in user docs:

- **Project filter:** `icm transcript search "<q>" --project <name>`.
- **Agent filter:** session `agent` = adapter name (returned in results).
- **FTS tag/bundle tokens:** `icm transcript search "llmenv-tag:rust"` →
  the scope-header message → its session.
- **Structured metadata:** full `{tags, bundles, scopes, cwd, adapter,
  llmenv_version}` JSON on the session for exact inspection/replay.

## Testing

- Config: an absent `session_log` block yields `{file: false, transcript:
  true, verbose: false}`; explicit mapping parses and round-trips; a
  bare-string `session_log` is rejected with the migration message.
- Scope-header token formatting — property test reusing the `llmenv-tag` /
  `llmenv-bundle` convention and the `validate_*` guards.
- `SessionLogEvent` → file JSONL line and → `icm transcript record` arg vector
  are consistent (same content/role).
- `claude_session_id → icm_session_id` map: store/recall round-trip, 0o600
  perms (property test, mirrors the existing `icm.rs` perms test).
- Graceful skip when `icm` is absent (mock a missing binary → no error, no
  output).
- Hook payload (stdin JSON) → event mapping for each of the verbose events.
- Content truncation at `max_content_bytes` boundary.

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
