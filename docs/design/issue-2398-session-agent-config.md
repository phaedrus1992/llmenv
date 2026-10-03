# Issue #2398 — per-session agent-config document in the state dir

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2398
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (hook-run side effect, one new hook event, one new section in `task session summary`)
- **Related:** #2339 (resume context on sessions and tasks, shipped), #2142 (SessionStart context, shipped)
- **Report:** `docs/reference/pi-durable-evaluation.md` §4 (multi-project row), §9 item 6

This is a spec, not a plan.

## Problem

Nothing in llmenv records what a session runs as: engine, model, effort level, cwd, active tags and bundles, config hash.
After a `/clear`, a compaction, or a resume, a fresh agent cannot tell.
A human reading the state dir cannot tell either.
pi-durable keeps a per-conversation agent config for this reason.

## Claude Code facts (hooks reference, fetched 2026-10-02)

| Fact | Detail |
| --- | --- |
| Common hook input fields | `session_id`, `transcript_path`, `cwd`, `permission_mode`, `hook_event_name`, `prompt_id` (2.1.196+), `effort` (an object with `level`, present in tool-use context when the model supports effort), `agent_id` and `agent_type` (subagents) |
| `SessionStart` extra fields | `source` (`startup`, `resume`, `clear`, `compact`, `fork`), `model` (canonical name such as `claude-opus-5`), `agent_type`, `session_title`; on `resume` and `fork` with 2.1.251+: `seconds_since_last_response`, `context_tokens`, `prompt_cache_likely_expired`, `estimated_cache_write_usd` |
| `PostModelSwitch` | Added in 2.1.251. Input: `from_model`, `to_model`, `reason` (`user_requested`, `auto_recovery`, `session_restored`). Observational; accepts `hookSpecificOutput.additionalContext` and `systemMessage`. The matcher runs on the canonical name of `to_model` |
| `PreModelSwitch` | Same input; can block. Not used by this issue |

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `HookEvent` (snake_case `FromStr` and `Display`) has no model-switch variant; `run()` reads stdin JSON, `hook_event_name`, `session_id`, and `CLAUDE_CODE_VERSION`, then calls `run_inner` | `src/hook_run/mod.rs` |
| `source` is read as `payload["source"]` in `continues_session`, `resets_read_state`, and `adaptive.rs`; `model` is never read | `src/hook_run/mod.rs`, `src/hook_run/adaptive.rs` |
| `CLAUDE_CODE_HOOK_EVENTS` lists the supported events; `BASELINE_HOOK_EVENTS` (`session_start`, `session_end`) are always registered; `generate_settings_json` writes the hooks; `lifecycle_hook_registrations` lists them for doctor | `src/adapter/claude_code/mod.rs` |
| Per-session state files: `read_once`, `repeat_detect`, `slippage`, `recall_session` each keep `state_dir/<feature>/{session_id}.json`; shared helpers `prune_stale_json_files`, `context_was_lost`, `unix_now` | `src/hook_run/session_state.rs` |
| The most careful writer is `session_ledger::LedgerStore`: validates the id with `paths::is_valid_short_name`, creates the dir with `create_dir_owner_only`, writes with `write_owner_only_atomic`, prunes at 7 days | `src/hook_run/session_ledger.rs` |
| Scope data at hook time: `scope::matcher::Env::detect_for_config`, `scope::evaluate` → `ActiveScopes`; `build_scope_context(active, tags, bundles, cwd, adapter_name, claude_code_version)` → `ScopeContext { tags, bundles, project, cwd, adapter, llmenv_version, claude_code_version }` | `src/hook_run/mod.rs`, `src/session_log/scope_header.rs` |
| Config hash: `materialize::cache::hash_manifest(&manifest)`; the booted hash is `CacheManifest::read(dir).content_hash` (see `run_check_stale`) | `src/materialize/cache.rs`, `src/materialize/manifest.rs`, `src/cli/mod.rs` |
| Engine: `adapter.name()` is hyphenated (`claude-code`), `adapter::engine_id(adapter)` is the config form (`claude_code`) | `src/adapter/mod.rs` |
| Effort: config-side only (`effort_level`, `model_effort`); no runtime source today | `crates/llmenv-config/src/schema.rs`, `src/adapter/model_settings.rs` |
| SessionStart output: `run_inner` emits the wake-up text with the `mcp_health::session_start_notice` text in front, through `AgentAdapter::emit_hook_context`, wrapped by `SESSION_START_CONTEXT_HEADER` | `src/hook_run/mod.rs`, `src/adapter/mod.rs` |
| `task session summary` → `session::session_summary(state_dir, id) -> SessionSummary { id, name, description, done, total, tasks, resume }`; human form `render_task_session_summary_human`; `Session.owner_session: Option<String>` is the engine session id, private today | `src/task/session.rs`, `src/cli/mod.rs` |

## Decisions

1. **One JSON document per engine session**, at `state_dir/agent_config/{session_id}.json`.
   The session id is Claude Code's `session_id` from the hook payload.
   Same directory rules as `LedgerStore`: owner-only dir, atomic write, id validated with `is_valid_short_name`.
2. **Written on every `SessionStart`**, whatever the `source`.
   The write replaces the file; a `compact` or `clear` keeps the same session id and only refreshes `updated_at` and `source`.
3. **Updated on `PostModelSwitch`.**
   New `HookEvent::PostModelSwitch`, registered in `BASELINE_HOOK_EVENTS` with no matcher.
   The handler reads `to_model` and `reason`, loads the document, sets `model`, appends `{at, from_model, to_model, reason}` to a `model_history` list capped at 20 entries, and writes it back.
   It emits no context (an empty stdout).
   If the document is missing (hook added mid-session), create it with what the payload gives.
4. **Read back only on `resume` and `compact`.**
   On `startup` and `clear` the model has no earlier state to lose.
   On `fork` the new session has its own id and gets its own document from this same `SessionStart`.
   The injected text is one line, placed before the wake-up block: `[llmenv session] engine <engine>, model <model>, effort <level or unset>, project <project>, tags <a, b>, config <hash prefix>`.
   This is session metadata, not memory, so it does not use the ICM header.
5. **Effort comes from the payload when present, else from config.**
   `effort.level` is in the common fields when the model supports effort; otherwise use the resolved `effort_level` for the engine, else `unset`.
6. **`task session summary` shows the agent block when it can find one.**
   `Session` gets an accessor `owner_session()`; `session_summary` reads `agent_config/{owner_session}.json` and fills an `agent: Option<AgentConfig>` field.
   The human renderer prints it as a `running as` line under the header; JSON carries the object.
   No agent block when the session has no owner or the file is missing.
7. **Prune at 7 days** with `prune_stale_json_files`, on each write, as the other per-session files do.
8. **No new config.** The document is always written when the Claude Code adapter runs hooks; it is small and owner-only.

## Design

### Module `src/hook_run/agent_config.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentConfig {
    pub engine: String,          // config form, e.g. "claude_code"
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: String,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub bundles: Vec<String>,
    pub config_hash: Option<String>,
    pub llmenv_version: String,
    pub engine_version: Option<String>,
    pub source: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub model_history: Vec<ModelSwitch>,
}
```

Functions:

- `path(state_dir, session_id) -> Option<PathBuf>` (None for an invalid id).
- `write_session_start(state_dir, session_id, AgentConfig)`.
- `apply_model_switch(state_dir, session_id, to_model, reason, at)`.
- `load(state_dir, session_id) -> Option<AgentConfig>` (corrupt file reads as None, logged at debug).
- `resume_line(&AgentConfig) -> String` (pure).
- `from_scope_context(&ScopeContext, engine, model, effort, config_hash, source, now) -> AgentConfig` (pure).

Serialization of the document is the only file format; keep it stable, since a human reads it.

### Hook-run wiring (`src/hook_run/mod.rs`)

- Add `PostModelSwitch` to `HookEvent`.
  `run_inner` handles it before the memory pipeline: call `apply_model_switch`, print nothing, return.
  It must sit in the #702 early-exit list so no ICM call happens.
- On `SessionStart`, after `build_scope_context` and before the output is assembled: build the document and write it.
  When `source` is `resume` or `compact`, prepend `resume_line` to the SessionStart output text, before the health notice.
- The write is fail-soft: an error logs at debug and the hook continues.

### Adapter (`src/adapter/claude_code/mod.rs`)

- Add `PostModelSwitch` to `CLAUDE_CODE_HOOK_EVENTS` and `BASELINE_HOOK_EVENTS`.
- `lifecycle_hook_registrations` lists it so doctor shows it.
- Crush and opencode do not register it.

### Task summary (`src/task/session.rs`, `src/cli/mod.rs`)

- `SessionSummary.agent: Option<AgentConfig>`.
- Human render: `running as claude_code claude-opus-5, effort high` on its own line after the header.

### Docs

- `website/docs/commands.md`, `## task`, under `### Resume context`: the agent block, `(added in v3.12.0)`.
- The hooks section of `website/docs/engines.md` (or wherever the Claude Code hook list lives): `PostModelSwitch` is registered, what it writes, and the one-line resume context.
- Changelog `Added`: per-session agent-config document under the state dir; one-line "running as" context on resume and compact; `task session summary` shows it.

## Tests

1. `from_scope_context` fills every field; `resume_line` renders `unset` for a missing effort and a missing model.
2. Write then load round-trips; an invalid session id gives `None` and writes nothing; a corrupt file loads as `None`.
3. `apply_model_switch` on a present file updates `model` and appends history; on a missing file creates a document with `model` set; history is capped at 20.
4. `run_inner` with `SessionStart` and each `source`: the file exists afterwards; the output starts with `[llmenv session]` only for `resume` and `compact`.
5. `run_inner` with `PostModelSwitch` prints nothing and makes no MCP call (use the existing fail-soft test harness that counts client calls).
6. Adapter: `generate_settings_json` registers `PostModelSwitch` once, with no matcher; crush and opencode output unchanged.
7. `session_summary` with an `owner_session` whose file exists carries the agent block; without one it is `None`.
8. Property test: for any `AgentConfig`, serialize → deserialize is identity.

## Acceptance criteria

1. After a new Claude Code session starts, `state_dir/agent_config/<session_id>.json` exists with engine, model, tags, and config hash filled.
2. `/model` to another model updates `model` in the file and adds a history entry.
3. A `claude --resume` session shows the `[llmenv session]` line in its first context; a `startup` session does not.
4. `llmenv task session summary` on a session with an owner prints the `running as` line.
5. Changelog entry and docs with the version tag.

## Out of scope

- Writing the document for Crush or opencode (no `model` in their payloads is confirmed).
- Using `PreModelSwitch` to block a switch.
- Routing engine or model from config (v4.1.0, #2394).
