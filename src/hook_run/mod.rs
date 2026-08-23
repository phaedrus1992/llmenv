//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

pub(crate) mod action;
pub(crate) mod cbm_index_guard;
pub(crate) mod cd_guard;
pub(crate) mod detached_consolidation;
pub(crate) mod detached_store;
mod launch_client;
pub(crate) mod mcp_client;
pub(crate) mod read_once;
pub(crate) mod repeat_detect;
mod session_state;
pub(crate) mod slippage;
pub(crate) mod task_tools;
pub(crate) mod transcript;

use std::io::Write;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use std::collections::HashMap;

use action::Action;
use anyhow::Context as _;
use mcp_client::McpHttpClient;
use serde_json::json;
use tracing::{debug, error, warn};

use crate::config::SessionLog;
use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::mcp::resolve::{ResolvedKind, resolve_mcps};
use crate::session_log::dispatch as transcript_dispatch;
use crate::session_log::event::{EventKind, EventScope, SessionLogEvent, now_rfc3339};
use crate::session_log::{ScopeContext, scope_header_content, scope_metadata_json, state};
use llmenv_config::LogLevel;

/// A single cross-project, tag-scoped recall the TurnStart hook issues against
/// ICM. Exposes the recall contract (#197) so it is testable without a live
/// MCP backend: each query is **project-unfiltered** (`project: ""`) and keyed
/// on `llmenv-tag:<tag>`, so memory stored under that tag in any project
/// surfaces when the tag activates here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecallQuery {
    /// The active tag this recall targets.
    pub tag: String,
    /// The `llmenv-tag:<tag>` keyword the recall is keyed on.
    pub keyword: String,
}

/// Build the cross-project tag recall queries for a set of active tags.
/// One query per tag, in input order. Tags are validated first; an invalid tag
/// aborts the whole set so a malformed scope can't inject recall metacharacters.
///
/// # Errors
/// Returns an error if any tag fails [`validate_tag`].
pub fn tag_recall_queries(tags: &[String]) -> anyhow::Result<Vec<TagRecallQuery>> {
    if tags.is_empty() {
        debug!("no tags configured for recall");
        return Ok(Vec::new());
    }
    debug!(tag_count = tags.len(), "building tag recall queries");
    tags.iter()
        .map(|tag| {
            validate_tag(tag).map_err(|e| {
                warn!(tag = %tag, error = %e, "tag name validation failed");
                e
            })?;
            debug!(tag = %tag, "tag recall query created");
            Ok(TagRecallQuery {
                tag: tag.clone(),
                keyword: action::tag_keyword(tag),
            })
        })
        .collect()
}

/// A single cross-project, bundle-scoped recall the TurnStart hook issues
/// against ICM. Mirrors [`TagRecallQuery`] for bundles (#228): each query is
/// **project-unfiltered** (`project: ""`) and keyed on
/// `llmenv-bundle:<bundle>`, so memory stored under that bundle in any project
/// surfaces when the bundle activates here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRecallQuery {
    /// The active bundle this recall targets.
    pub bundle: String,
    /// The `llmenv-bundle:<bundle>` keyword the recall is keyed on.
    pub keyword: String,
}

/// Build the cross-project bundle recall queries for a set of active bundles.
/// One query per bundle, in input order. Bundle names are validated first; an
/// invalid name aborts the whole set so a malformed bundle can't inject recall
/// metacharacters.
///
/// # Errors
/// Returns an error if any bundle name fails [`validate_bundle`].
pub fn bundle_recall_queries(bundles: &[String]) -> anyhow::Result<Vec<BundleRecallQuery>> {
    if bundles.is_empty() {
        debug!("no bundles configured for recall");
        return Ok(Vec::new());
    }
    debug!(
        bundle_count = bundles.len(),
        "building bundle recall queries"
    );
    bundles
        .iter()
        .map(|bundle| {
            validate_bundle(bundle).map_err(|e| {
                warn!(
                    bundle = %bundle,
                    error = %e,
                    "bundle name validation failed"
                );
                e
            })?;
            debug!(bundle = %bundle, "bundle recall query created");
            Ok(BundleRecallQuery {
                bundle: bundle.clone(),
                keyword: action::bundle_keyword(bundle),
            })
        })
        .collect()
}

/// Per-call network timeout. Lifecycle hooks run on startup and every prompt, so
/// a slow/dead remote ICM must not stall the agent. 2s balances real round-trips
/// against not hanging the prompt.
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);

/// An engine-neutral lifecycle event. Adapters translate these to native hook
/// names when wiring them into agent config.
///
/// `SessionStart`/`TurnStart`/`SessionEnd` drive ICM memory recall/store (see
/// `dispatch`) and the baseline session log (see `handle_session_log`). The
/// rest drive per-turn session-log capture (see `event_to_log_kind`); they
/// carry no memory actions of their own — Claude's `UserPromptSubmit` native
/// hook fires both `TurnStart` (memory recall) and `UserPromptSubmit`
/// (session-log capture) as two separate handlers on the same event (see
/// adapter wiring, #382 Task 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Session begins (Claude Code: `SessionStart`).
    SessionStart,
    /// A user prompt/turn begins (Claude Code: `UserPromptSubmit`).
    TurnStart,
    /// Session ends (Claude Code: `SessionEnd`).
    SessionEnd,
    /// Post-session consolidation hook (R5). Runs after SessionEnd to
    /// trigger reflective consolidation on the accumulated conversation.
    PostSession,
    /// The raw prompt submission (Claude Code: `UserPromptSubmit`).
    UserPromptSubmit,
    /// Before a tool call (Claude Code: `PreToolUse`).
    PreToolUse,
    /// After a tool call (Claude Code: `PostToolUse`).
    PostToolUse,
    /// A UI notification fired (Claude Code: `Notification`).
    Notification,
    /// The main agent finished responding (Claude Code: `Stop`).
    Stop,
    /// A subagent finished responding (Claude Code: `SubagentStop`).
    SubagentStop,
    /// About to compact the transcript (Claude Code: `PreCompact`).
    PreCompact,
}

impl FromStr for HookEvent {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session_start" => Ok(HookEvent::SessionStart),
            "turn_start" => Ok(HookEvent::TurnStart),
            "session_end" => Ok(HookEvent::SessionEnd),
            "user_prompt_submit" => Ok(HookEvent::UserPromptSubmit),
            "post_session" => Ok(HookEvent::PostSession),
            "pre_tool_use" => Ok(HookEvent::PreToolUse),
            "post_tool_use" => Ok(HookEvent::PostToolUse),
            "notification" => Ok(HookEvent::Notification),
            "stop" => Ok(HookEvent::Stop),
            "subagent_stop" => Ok(HookEvent::SubagentStop),
            "pre_compact" => Ok(HookEvent::PreCompact),
            other => Err(anyhow::anyhow!(
                "unknown hook event '{other}' (expected session_start|turn_start|session_end|\
                 user_prompt_submit|pre_tool_use|post_tool_use|notification|stop|\
                 subagent_stop|pre_compact)"
            )),
        }
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::TurnStart => "turn_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::Notification => "notification",
            HookEvent::Stop => "stop",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::PostSession => "post_session",
        };
        f.write_str(s)
    }
}

/// The ordered actions to run for an event, given the active tags' and bundles'
/// recall queries (built by [`tag_recall_queries`] and [`bundle_recall_queries`],
/// the single sources of tag→recall and bundle→recall expansion), plus the
/// active memory entry's configured wake-up token budget (#1216, `None` if
/// unset), threaded straight into `Action::WakeUp` on `SessionStart`.
///
/// `TurnStart` runs the project-scoped natural-language `Recall` first, then one
/// project-unfiltered `RecallTag` per active tag (#197), then one
/// project-unfiltered `RecallBundle` per active bundle (#228). The turn-capture
/// events carry no memory actions.
fn dispatch(
    event: HookEvent,
    tag_queries: &[TagRecallQuery],
    bundle_queries: &[BundleRecallQuery],
    wakeup_max_tokens: Option<u32>,
) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => vec![Action::WakeUp(wakeup_max_tokens)],
        HookEvent::TurnStart => {
            let mut actions = vec![Action::Recall];
            actions.extend(tag_queries.iter().cloned().map(Action::RecallTag));
            actions.extend(bundle_queries.iter().cloned().map(Action::RecallBundle));
            actions
        }
        HookEvent::SessionEnd => vec![Action::Store],
        HookEvent::UserPromptSubmit
        | HookEvent::PreToolUse
        | HookEvent::PostToolUse
        | HookEvent::Notification
        | HookEvent::Stop
        | HookEvent::SubagentStop
        | HookEvent::PreCompact => vec![],
        HookEvent::PostSession => vec![], // consolidation runs as a separate step
    }
}

/// Maps a `HookEvent` to its session-log `(kind, role)`. `None` for
/// the lifecycle/memory events (`SessionStart`/`TurnStart`/`SessionEnd`),
/// which `handle_session_log` handles separately.
fn event_to_log_kind(event: HookEvent) -> Option<(EventKind, &'static str)> {
    match event {
        HookEvent::UserPromptSubmit => Some((EventKind::Prompt, "user")),
        HookEvent::PreToolUse => Some((EventKind::ToolUse, "tool")),
        HookEvent::PostToolUse => Some((EventKind::ToolResult, "tool")),
        HookEvent::Notification => Some((EventKind::Notification, "system")),
        HookEvent::Stop | HookEvent::SubagentStop => Some((EventKind::Stop, "assistant")),
        HookEvent::PreCompact => Some((EventKind::Notification, "system")),
        HookEvent::SessionStart | HookEvent::TurnStart | HookEvent::SessionEnd => None,
        HookEvent::PostSession => None, // consolidation runs as a separate step
    }
}

/// Extract `(tool_name, content)` for a hook event from Claude's hook stdin
/// payload. Field names per the Claude Code hooks reference: prompt text on
/// `UserPromptSubmit` is `prompt`; tool calls carry `tool_name` +
/// `tool_input` (`PreToolUse`) or `tool_input` + `tool_response`
/// (`PostToolUse`); `Notification` carries `message`; `Stop`/`SubagentStop`
/// carry `last_assistant_message`; `PreCompact` carries `trigger`.
fn event_content(event: HookEvent, payload: &serde_json::Value) -> (Option<String>, String) {
    match event {
        HookEvent::UserPromptSubmit => (
            None,
            payload["prompt"].as_str().unwrap_or_default().to_string(),
        ),
        HookEvent::PreToolUse => (
            payload["tool_name"].as_str().map(str::to_owned),
            json_or_empty(&payload["tool_input"]),
        ),
        HookEvent::PostToolUse => (
            payload["tool_name"].as_str().map(str::to_owned),
            json_or_empty(&payload["tool_response"]),
        ),
        HookEvent::Notification => (
            None,
            payload["message"].as_str().unwrap_or_default().to_string(),
        ),
        HookEvent::Stop | HookEvent::SubagentStop => (
            None,
            payload["last_assistant_message"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
        HookEvent::PreCompact => (
            None,
            payload["trigger"].as_str().unwrap_or_default().to_string(),
        ),
        HookEvent::SessionStart
        | HookEvent::TurnStart
        | HookEvent::SessionEnd
        | HookEvent::PostSession => (None, String::new()),
    }
}

/// Compact JSON for an object-typed field (tool input/response); "" when absent.
fn json_or_empty(v: &serde_json::Value) -> String {
    if v.is_null() {
        String::new()
    } else {
        v.to_string()
    }
}

/// Append `text` (the read_once advisory/deny result) to `out`. `run()`
/// treats the *entire* returned string as the deny reason once it detects
/// the `__DENY__:` prefix (see the `starts_with("__DENY__:")` check below),
/// so a deny always replaces whatever `out` already held rather than being
/// appended alongside it — mixing in other content would either corrupt the
/// reason or, if positioned wrong, make the prefix check miss the deny and
/// silently downgrade it to an allow. This is an always-on guard, not a
/// `debug_assert!`: a deny defeated in a release build is a silent,
/// security-relevant regression, not a test-time nicety. Dormant today only
/// because `dispatch(HookEvent::PreToolUse, ..)` always returns no actions
/// (#868), so `out` is empty in practice and this never discards anything.
fn append_read_once_result(out: &mut String, text: &str) {
    if text.starts_with("__DENY__:") {
        if !out.is_empty() {
            tracing::error!(
                discarded = %out,
                "hook-run: read_once deny computed alongside other pipeline \
                 output; discarding the other output to keep the deny intact"
            );
        }
        text.clone_into(out);
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}

/// Whether `engine`'s hook bridge decides a tool call was blocked from the
/// process exit code rather than the JSON envelope on stdout.
///
/// opencode's generated plugin shim checks `code === 2` and ignores stdout for
/// the block decision, so a deny that exits 0 there is silently allowed.
/// Claude Code honours the envelope itself, and exit 2 would only duplicate a
/// decision it has already made.
///
/// `engine` is [`crate::adapter::AgentAdapter::name`]'s hyphenated cache-dir
/// form, the same value `should_check_stale` takes — see the warning there.
fn blocks_by_exit_code(engine: &str) -> bool {
    engine == "opencode"
}

/// Whether a hook decision can be returned immediately, or has to fall through
/// so session logging still sees the event.
///
/// Both callers return the same text either way, so getting this wrong is
/// invisible in the output and shows up only as missing log lines — the
/// #231/#864 bug class, where an unconditional early return silently dropped
/// Debug-level capture for every call. Named so the condition itself is
/// testable rather than buried in a branch whose two arms look alike.
fn can_short_circuit(event: HookEvent, log_cfg: &crate::config::SessionLog) -> bool {
    let level = event_to_log_kind(event).map_or(LogLevel::Debug, |(kind, _)| kind.log_level());
    !log_cfg.any_sink_wants(level)
}

/// Whether `event` is the one that records a completed tool call for the
/// slippage metrics layer (#317).
///
/// Named rather than inlined so the event choice is testable: inside
/// `run_inner` it sits behind a payload read and a state-dir resolve, where a
/// wrong event is invisible until someone notices the counts are empty.
fn counts_tool_use(event: HookEvent) -> bool {
    event == HookEvent::PostToolUse
}

/// Whether `event` carries the per-turn rules digest (#317).
fn carries_turn_digest(event: HookEvent) -> bool {
    event == HookEvent::UserPromptSubmit
}

/// Whether `event` folds the session metrics summary into the stored chunk
/// (#317).
fn stores_session_metrics(event: HookEvent) -> bool {
    event == HookEvent::SessionEnd
}

/// Whether this `hook-run` invocation should also check for config drift (#741).
///
/// Claude Code only: the comparison baseline is the booted `CLAUDE_CONFIG_DIR`'s
/// manifest, which no other engine sets.
///
/// `engine` is [`crate::adapter::AgentAdapter::name`]'s hyphenated cache-dir
/// form (`claude-code`), *not* the underscored `--engine` id
/// ([`crate::adapter::engine_id`]). Comparing it against `claude_code` matches
/// nothing and silently disables the check, which is exactly the bug the
/// exhaustive test below exists to catch.
fn should_check_stale(event: HookEvent, engine: &str) -> bool {
    event == HookEvent::SessionStart && engine == "claude-code"
}

/// Whether the caller should exit 0 or signal a blocked tool call.
///
/// Exit code 2 is what both supported engines read as "don't run this tool":
/// Claude Code documents it as equivalent to a `deny` decision, and opencode's
/// generated plugin shim treats it as the only block signal (`code === 2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookExit {
    /// Nothing to block; exit 0.
    Success,
    /// A `PreToolUse` deny was emitted; exit 2.
    Block,
}

/// Append a pending `launch` mid-session notice (#1480) to `text`, joined by
/// a newline when `text` already has content so the two don't run together.
/// Returns `text` unchanged when there's no notice.
fn append_pending_notice(mut text: String, notice: Option<String>) -> String {
    let Some(notice) = notice else {
        return text;
    };
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&notice);
    text
}

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(HookExit::Success)` even when the backend is unreachable — only an
/// explicit deny asks the caller for a non-zero exit.
pub(crate) fn run(event: &str, engine: &str) -> anyhow::Result<HookExit> {
    use std::io::Read;

    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv hook-run: failed to read stdin: {e}");
    }
    let stdin_json = serde_json::from_str::<serde_json::Value>(&stdin_buf)
        .inspect_err(|e| tracing::warn!("hook-run: failed to parse stdin JSON: {e}"))
        .ok();
    let hook_event_name: &str = stdin_json
        .as_ref()
        .and_then(|v| v["hook_event_name"].as_str())
        .unwrap_or_default();
    let claude_session_id: Option<&str> =
        stdin_json.as_ref().and_then(|v| v["session_id"].as_str());
    let claude_code_version = std::env::var("CLAUDE_CODE_VERSION")
        .ok()
        .unwrap_or_default();

    let parsed = match HookEvent::from_str(event) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("llmenv: {e}");
            return Ok(HookExit::Success);
        }
    };
    let null_payload = serde_json::Value::Null;
    let payload = stdin_json.as_ref().unwrap_or(&null_payload);
    // Not fail-soft, unlike everything below it: running a hook against the
    // wrong engine's config is worse than not running it (#1386). The CLI
    // boundary rejects an unknown `--engine` before this, so reaching the error
    // arm means an internal caller passed one — surface it rather than sniffing.
    let adapter = crate::adapter::adapter_for_engine(engine)
        .ok_or_else(|| crate::adapter::unknown_engine_error(engine))?;

    // #741: the drift check runs from here rather than from its own
    // `SessionStart` hook. Both were registered unconditionally on the same
    // event, so a session start spawned two `llmenv` processes that each parsed
    // the config; folding it in leaves one, and leaves one place where "does
    // session start check for drift" is decided.
    //
    // Fail-soft on purpose — a hook that can't answer "has the config drifted"
    // must not take down memory wake-up or session logging with it.
    if should_check_stale(parsed, adapter.name()) {
        // A hook's stderr is piped to the agent, never a terminal, so colors
        // would only add escape codes to the model's context.
        let use_color = crate::cli::should_use_color(None, false);
        if let Err(e) = crate::cli::run_check_stale(use_color, false) {
            // Visible, not `tracing::debug!`: the default `EnvFilter` is
            // `ERROR`, so a debug line here would mean the user silently loses
            // drift detection with no way to notice (#1345's lesson).
            eprintln!("warning: llmenv could not check whether your config drifted: {e:#}");
        }
    }

    match run_inner(
        parsed,
        claude_session_id,
        payload,
        adapter.name(),
        &claude_code_version,
    ) {
        Ok(text) => {
            // #318: deny envelope detected — write a proper deny JSON envelope
            // to stdout so the Claude Code engine blocks the tool call.
            if text.starts_with("__DENY__:") {
                let reason = text.trim_start_matches("__DENY__:");
                let envelope = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        // Claude Code's field is `permissionDecisionReason`;
                        // it was `deniedReason` here, which the engine doesn't
                        // read — the call was blocked but the model was never
                        // told why, so it had no way to do anything but retry.
                        "permissionDecisionReason": reason,
                    }
                });
                if let Err(e) = writeln!(std::io::stdout(), "{envelope}")
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("llmenv: failed to write hook output: {e}");
                }
                if blocks_by_exit_code(adapter.name()) {
                    // opencode's shim reads nothing but the exit code, so the
                    // envelope above is inert there and the reason has to go
                    // to stderr to reach anyone. Claude Code is left on exit 0
                    // deliberately: it already blocks on the envelope alone,
                    // and exiting 2 there would change a working path for no
                    // gain.
                    eprintln!("{reason}");
                    return Ok(HookExit::Block);
                }
            } else {
                let text = append_pending_notice(text, launch_client::check_pending_notice());
                let out = adapter.emit_hook_context(hook_event_name, &text);
                if !out.is_empty()
                    && let Err(e) = writeln!(std::io::stdout(), "{out}")
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("llmenv: failed to write hook output: {e}");
                }
            }
        }
        Err(e) => {
            eprintln!("llmenv: memory {event} skipped: {e}");
        }
    }
    Ok(HookExit::Success)
}

/// Resolve config, find the memory URL, run the event's actions, and return the
/// concatenated result text. Errors here are caught and warned by `run`.
///
/// The memory backend (recall/store) and session logging are independent: a
/// missing/unreachable memory MCP skips memory actions but must not prevent
/// the file-sink session log from being written (see `handle_session_log`).
/// `main()` loads config once (before the tracing subscriber is set up, to
/// resolve session-log settings) and stashes it here so `load_cached_config`
/// can reuse it instead of re-parsing `config.yaml` a second time in the same
/// process. Direct callers that never went through `main()` (tests, other
/// entrypoints) fall back to loading from `path` normally.
///
/// `Mutex<Option<_>>` rather than `OnceLock` (#881): a `OnceLock` can never be
/// cleared once set, which would make the preload permanent for the rest of
/// any process that embeds llmenv as a library and calls an entrypoint more
/// than once — and it can't be reset between test runs either. The mutex costs
/// nothing measurable here (this is read at most a few times per process,
/// nowhere near a hot loop) and buys `reset_preloaded_config_for_test` below.
static PRELOADED_CONFIG: Mutex<Option<crate::config::Config>> = Mutex::new(None);

/// Stash a config already loaded by `main()` for reuse by `load_cached_config`.
///
/// Must only be called once per process: a second call is a no-op if a config
/// is already stashed. `main()` is the only caller today and calls it at most
/// once.
pub fn set_preloaded_config(config: crate::config::Config) {
    let mut slot = PRELOADED_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(config);
    }
}

/// Clear the preloaded config so a test doesn't observe another test's
/// `set_preloaded_config` call. Test-only: production code always runs in a
/// fresh process, so there is nothing to reset outside `cargo test`'s shared
/// binary.
#[cfg(test)]
fn reset_preloaded_config_for_test() {
    *PRELOADED_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Load config from `path`, reusing `main()`'s preload when available. Also
/// used by non-hook-run CLI commands (`export`, `regenerate`, `statusline`)
/// that would otherwise re-parse the same `config.yaml` main() already loaded.
/// Every other CLI command intentionally calls `Config::load` directly instead
/// (#880): each is a one-shot command in its own process, so there's no second
/// in-process parse to skip, and routing it through this cache would add
/// indirection with no measurable benefit.
///
/// Once a preload exists, `path` is ignored — every current caller resolves
/// the same canonical `paths::config_path()` that `main()` preloaded from, so
/// this is safe today, but a caller passing a *different* path would silently
/// get main()'s config instead of loading its own.
pub(crate) fn load_cached_config(path: &std::path::Path) -> anyhow::Result<crate::config::Config> {
    if let Some(config) = PRELOADED_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        return Ok(config.clone());
    }
    crate::config::Config::load(path)
}

/// Decide the `PreToolUse` decision text, if any, from the three
/// mutually-exclusive interceptors: the #985 task-tool redirect
/// (TaskCreate/TaskList/TaskUpdate → `llmenv task`) and the #318/#864
/// read-once dedup (Read) — plus the #1006 repeat-call detector, which is
/// *not* mutually exclusive with the other two: it observes every
/// `PreToolUse` call, including ones the primary interceptors already
/// decided about.
///
/// This matters because the single biggest real-world trigger for a
/// stuck-loop model isn't an uncovered tool like `Bash` — it's the model
/// repeatedly retrying `TaskCreate`/`TaskList`/`TaskUpdate` after
/// `task_tools` denies and redirects it each time. If `repeat_detect` only
/// ran as a fallback *after* the primary arms (as it originally did), that
/// exact case — llmenv's own redirect being ignored on repeat — would be
/// invisible to it, since `task_tools` always wins the primary decision for
/// those three tools. So `repeat_detect` is computed independently and its
/// warning is appended to whatever the primary arms decided, not gated on
/// them staying silent.
///
/// `read_once`'s handler returns `""` (empty) as its pass-through case, so a
/// naive `if <enabled>` gate on it alone — with no check on the handler's
/// own result — would take that arm on every call once the feature was on.
/// `task_tools`'s handler avoids this by returning `Option` directly.
///
/// Combining order is safety-relevant: only `primary` can ever carry the
/// `__DENY__:` sentinel (`repeat_detect` never denies, only warns — see its
/// module doc), so `primary` must always come first in the concatenation.
/// `append_read_once_result` further down treats the *entire* returned
/// string as the deny reason once it detects that prefix at position 0;
/// putting `repeat_detect`'s text first would still keep the prefix at 0 in
/// practice (nothing precedes it), but ordering primary-first documents the
/// invariant explicitly rather than relying on that coincidence.
/// The whole `PreToolUse` decision, including the parts that need no state
/// dir. `state_dir` is threaded in as the `Result` the caller got so a
/// failure to resolve it stays a *degradation* rather than an abort — the
/// pre-#1089 behaviour, kept because propagating it would also drop the
/// task-tracker redirect and any session logging below (#231/#864).
///
/// #1331's guard is resolved first and alone. It reads only the call's
/// arguments, so a state-dir failure must not be able to disable a deny whose
/// job is stopping one project's index from overwriting another's. And a deny
/// only counts when `__DENY__:` leads the string (see
/// `append_read_once_result`), so folding it in beside `repeat_detect` — which
/// matches any tool, `index_repository` included — would let an advisory
/// prepend itself and silently downgrade the deny to an allow.
fn resolve_pre_tool_decision(
    stdin_payload: &serde_json::Value,
    claude_session_id: Option<&str>,
    config: &crate::config::Config,
    task_tracker_enabled: bool,
    state_dir: anyhow::Result<std::path::PathBuf>,
) -> Option<String> {
    let clobber_deny = crate::hook_run::cbm_index_guard::handle_pre_tool_use(stdin_payload);
    if !clobber_deny.is_empty() {
        return Some(clobber_deny);
    }
    match state_dir {
        Ok(state_dir) => resolve_pre_tool_text(
            stdin_payload,
            claude_session_id,
            config,
            task_tracker_enabled,
            &state_dir,
        ),
        Err(e) => {
            error!(
                error = %e,
                "failed to resolve state_dir; read_once/repeat_detect skipped for this call \
                 and the task-tool redirect degraded to a deny"
            );
            task_tracker_enabled
                .then(|| crate::hook_run::task_tools::deny_tracker_unavailable(stdin_payload, &e))
                .flatten()
        }
    }
}

fn resolve_pre_tool_text(
    stdin_payload: &serde_json::Value,
    claude_session_id: Option<&str>,
    config: &crate::config::Config,
    task_tracker_enabled: bool,
    state_dir: &std::path::Path,
) -> Option<String> {
    // #317: resolved before the other layers because it is the only one here
    // that can deny, and a deny only counts when `__DENY__:` leads the string
    // (see `append_read_once_result`). Folding it in beside `repeat_detect`,
    // which matches every tool, would let an advisory prepend itself and
    // silently downgrade the deny to an allow.
    // #317 phase 3: checked before the write guard only because both deny and
    // the first deny wins; neither ordering is load-bearing beyond that.
    let scan_deny = crate::hook_run::slippage::handle_transcript_scan(
        config.features.as_ref().and_then(|f| f.slippage.as_ref()),
        stdin_payload,
    );
    if !scan_deny.is_empty() {
        return Some(scan_deny);
    }

    let write_guard = crate::hook_run::slippage::handle_pre_tool_use(
        config.features.as_ref().and_then(|f| f.slippage.as_ref()),
        stdin_payload,
        claude_session_id,
        state_dir,
    );
    if !write_guard.is_empty() {
        return Some(write_guard);
    }

    let primary = if task_tracker_enabled
        && let Some(t) = crate::hook_run::task_tools::handle_pre_tool_use(stdin_payload, state_dir)
    {
        Some(t)
    } else if let Some(ref features) = config.features
        && let Some(ref read_once) = features.read_once
        && read_once.enabled
    {
        let ro_text = crate::hook_run::read_once::handle_pre_tool_use(
            stdin_payload,
            claude_session_id,
            read_once,
            state_dir,
        );
        (!ro_text.is_empty()).then_some(ro_text)
    } else {
        None
    };

    // On by default (#1006): absent `features.repeat_detect` resolves the
    // same as an explicit, empty block — see `RepeatDetect::default()`.
    let repeat_detect_cfg = config
        .features
        .as_ref()
        .and_then(|f| f.repeat_detect.clone())
        .unwrap_or_default();
    let repeat_detect_text = repeat_detect_cfg.enabled.then(|| {
        crate::hook_run::repeat_detect::handle_pre_tool_use(
            stdin_payload,
            claude_session_id,
            &repeat_detect_cfg,
            state_dir,
        )
    });
    let repeat_detect_text = repeat_detect_text.filter(|t| !t.is_empty());

    // On by default (#976): absent `features.cd_guard` resolves the same as
    // an explicit, empty block — see `CdGuard::default()`.
    let cd_guard_cfg = config
        .features
        .as_ref()
        .and_then(|f| f.cd_guard.clone())
        .unwrap_or_default();
    let cd_guard_text =
        crate::hook_run::cd_guard::handle_pre_tool_use(stdin_payload, &cd_guard_cfg);
    let cd_guard_text = (!cd_guard_text.is_empty()).then_some(cd_guard_text);

    let parts: Vec<String> = [primary, repeat_detect_text, cd_guard_text]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The task-tracker's `Stop` reminder, with repeat-detection applied: once
/// the identical reminder has fired `threshold` times in a row for this
/// session, `repeat_detect::handle_stop` appends a pointer to `llmenv task
/// wait` (see #1006 — this is the same on-by-default detector as
/// `resolve_pre_tool_text`, applied to Stop-event nagging instead of
/// `PreToolUse` tool-call repetition).
fn resolve_stop_reminder(
    state_dir: &std::path::Path,
    claude_session_id: Option<&str>,
    config: &crate::config::Config,
) -> String {
    let mut reminder = crate::task::stop_hook_reminder(state_dir);
    // #317: appended before repeat-detect wraps the text, so a repeat warning
    // stays the last thing read — it's about the turn that just happened,
    // while the checklist is about what to do before ending.
    let critique = crate::hook_run::slippage::handle_stop(
        config.features.as_ref().and_then(|f| f.slippage.as_ref()),
    );
    if !critique.is_empty() {
        if !reminder.is_empty() {
            reminder.push_str("\n\n");
        }
        reminder.push_str(&critique);
    }
    let repeat_detect_cfg = config
        .features
        .as_ref()
        .and_then(|f| f.repeat_detect.clone())
        .unwrap_or_default();
    if repeat_detect_cfg.enabled {
        crate::hook_run::repeat_detect::handle_stop(
            &reminder,
            claude_session_id,
            &repeat_detect_cfg,
            state_dir,
        )
    } else {
        reminder
    }
}

/// Build the `LLMENV_TRACE_TIMING` marker payload from whichever phase
/// boundaries `run_inner` reached before returning. `t0`/`t_config` are
/// always available (computed before any early return); `t_scope`/`t_chunk`/
/// `t_end` are `None` on a path that returned before reaching them. Each
/// field is included only when its Instant is present, so an early return
/// still reports the phases it actually ran through instead of nothing
/// (#1128: previously only events reaching the full memory-dispatch path —
/// 4 of 11 — emitted this marker at all).
fn trace_timing_json(
    t0: std::time::Instant,
    t_config: std::time::Instant,
    t_scope: Option<std::time::Instant>,
    t_chunk: Option<std::time::Instant>,
    t_end: Option<std::time::Instant>,
) -> serde_json::Value {
    // Cap rather than panic on the (unreachable) overflow of an in-process
    // Instant delta past u64::MAX microseconds (~585,000 years).
    let us = |d: std::time::Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
    let mut fields = serde_json::Map::new();
    fields.insert(
        "config_load_us".to_string(),
        json!(us(t_config.saturating_duration_since(t0))),
    );
    if let Some(t_scope) = t_scope {
        fields.insert(
            "scope_eval_us".to_string(),
            json!(us(t_scope.saturating_duration_since(t_config))),
        );
        if let Some(t_chunk) = t_chunk {
            fields.insert(
                "prep_us".to_string(),
                json!(us(t_chunk.saturating_duration_since(t_scope))),
            );
            if let Some(t_end) = t_end {
                fields.insert(
                    "mcp_us".to_string(),
                    json!(us(t_end.saturating_duration_since(t_chunk))),
                );
            }
        }
    }
    serde_json::Value::Object(fields)
}

/// Emit the per-phase timing marker to stderr when `LLMENV_TRACE_TIMING` is
/// set (any value): exactly one line, `llmenv-trace <json>`. The clock always
/// runs (`Instant::now` is ~20ns); only emission is gated, so normal runs are
/// unaffected and stdout is never touched. See [`trace_timing_json`] for
/// which fields appear depending on how far the caller got.
fn emit_trace_timing(
    t0: std::time::Instant,
    t_config: std::time::Instant,
    t_scope: Option<std::time::Instant>,
    t_chunk: Option<std::time::Instant>,
    t_end: Option<std::time::Instant>,
) {
    if std::env::var_os("LLMENV_TRACE_TIMING").is_some() {
        eprintln!(
            "llmenv-trace {}",
            trace_timing_json(t0, t_config, t_scope, t_chunk, t_end)
        );
    }
}

fn run_inner(
    event: HookEvent,
    claude_session_id: Option<&str>,
    stdin_payload: &serde_json::Value,
    adapter_name: &str,
    claude_code_version: &str,
) -> anyhow::Result<String> {
    let t0 = std::time::Instant::now();
    let config_path = crate::paths::config_path()?;
    let config = load_cached_config(&config_path)?;
    let t_config = std::time::Instant::now();
    let log_cfg = config.session_log_resolved();

    // Whether the task tracker is enabled — hoisted here because the #985
    // task-tool redirect below gates on it, and the Stop reminder further down
    // reuses it.
    let task_tracker_enabled = config
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled);

    // PreToolUse text decision, shared by three mutually-exclusive
    // interceptors, checked in order: the #985 task-tool redirect
    // (TaskCreate/TaskList/TaskUpdate → `llmenv task`), the #318/#864
    // read-once dedup (Read), and the #1006 repeat-call detector (any tool,
    // fallback when neither of the above already fired). All are computed
    // before scope/memory resolution (none need it) and folded into one
    // `pre_tool_text`, so the shared session-log handling below applies to
    // all three: take the early-return fast path only when session-log has
    // no interest in PreToolUse at Debug level (EventKind::ToolUse's level);
    // otherwise fall through so `run_session_log` still runs and the decision
    // text is appended to `out` further down. Never unconditionally
    // short-circuit, or enabling any of them would silently drop Debug-level
    // session logging for every PreToolUse event (the #231/#864
    // early-return-drops-logging bug class).
    let pre_tool_text = if event == HookEvent::PreToolUse {
        // `state_dir()` is passed in rather than resolved there so its failure
        // stays a degradation instead of an abort — see the doc comment on
        // `resolve_pre_tool_decision` for why that matters and what it costs.
        let text = resolve_pre_tool_decision(
            stdin_payload,
            claude_session_id,
            &config,
            task_tracker_enabled,
            crate::paths::state_dir(),
        );
        match text {
            Some(t) => {
                // Shares `can_short_circuit` with the turn digest below, so
                // the level comes from the same `event_to_log_kind` mapping
                // `run_session_log` uses rather than a hardcoded
                // `LogLevel::Debug` that would drift if `EventKind::ToolUse`'s
                // level ever changed.
                if can_short_circuit(event, &log_cfg) {
                    emit_trace_timing(t0, t_config, None, None, None);
                    return Ok(t);
                }
                Some(t)
            }
            None => None,
        }
    } else {
        None
    };

    // #317: the per-turn rules digest. Computed here, beside `pre_tool_text`,
    // for the same reason: it needs no scope/memory resolution, so it must
    // survive the #702 early-exit below rather than being stranded behind it
    // when nothing else wants this event.
    // #317: counted here rather than inside the pipeline below, which the
    // #702 early-exit can skip entirely — a metric that only accrues when
    // something else happens to want the event would undercount silently.
    if counts_tool_use(event)
        && let Ok(state_dir) = crate::paths::state_dir()
    {
        crate::hook_run::slippage::handle_post_tool_use(
            config.features.as_ref().and_then(|f| f.slippage.as_ref()),
            stdin_payload,
            claude_session_id,
            &state_dir,
        );
    }

    let turn_text = if carries_turn_digest(event) {
        let text = crate::hook_run::slippage::handle_turn(
            config.features.as_ref().and_then(|f| f.slippage.as_ref()),
        );
        if text.is_empty() {
            None
        } else {
            if can_short_circuit(event, &log_cfg) {
                emit_trace_timing(t0, t_config, None, None, None);
                return Ok(text);
            }
            Some(text)
        }
    } else {
        None
    };

    // #231: the task tracker's Stop reminder is computed before the #702
    // early-exit (below) so it can take the cheap fast path when session-log
    // has no interest in Stop, and be appended to `out` further down when
    // session-log *does* want Stop — never unconditionally short-circuiting, or
    // enabling task_tracker would silently drop Stop-event session logging
    // (that early-return shape was tried and reverted; see the git history).
    if event == HookEvent::Stop && task_tracker_enabled && !log_cfg.any_sink_enabled() {
        let state_dir = crate::paths::state_dir()?;
        let reminder = resolve_stop_reminder(&state_dir, claude_session_id, &config);
        emit_trace_timing(t0, t_config, None, None, None);
        return Ok(reminder);
    }

    // #867: the rest of the pipeline (scope evaluation, tag/bundle recall
    // query validation, memory URL/MCP resolution, tokio runtime
    // construction) is fallible, and an error anywhere in it propagates via
    // `?` out of `run_inner` — which the caller (`run()`) degrades to "warn
    // on stderr, nothing on stdout". Without this wrapper, that would
    // silently discard an already-computed `read_once_text` (a deny/advisory
    // decision already made) whenever such an error fires after read_once
    // falls through here for Debug-level session logging. Wrapping it lets
    // the match below recover `read_once_text` on `Err` instead of losing it.
    let pipeline_result: anyhow::Result<String> = (|| {
        // #702: Early-exit for events that dispatch no memory actions AND have
        // no session-log consumer. The expensive work below (scope evaluation,
        // bundle merge, memory MCP resolution / HTTP client) is only needed when
        // dispatch produces actions (SessionStart/TurnStart/SessionEnd),
        // PostToolUse needs WebFetch auto-store, PostSession runs consolidation,
        // or session-log capture is active.
        if !matches!(
            event,
            HookEvent::SessionStart
                | HookEvent::TurnStart
                | HookEvent::SessionEnd
                | HookEvent::PostToolUse
                | HookEvent::PostSession
        ) && !log_cfg.any_sink_enabled()
        {
            emit_trace_timing(t0, t_config, None, None, None);
            return Ok(String::new());
        }

        let env = crate::scope::matcher::Env::detect_for_config(&config);
        let active = crate::scope::evaluate(&config, &env);
        let t_scope = std::time::Instant::now();

        // #365: register the active project with codebase-memory-mcp's own
        // background auto-watch on SessionStart. Deliberately NOT gated on an
        // ICM memory client being configured (unlike PostSession/PostToolUse
        // below) — read_once/codebase_memory are fully orthogonal to ICM.
        // Fire-and-forget: indexing a large repo can take minutes (the Linux
        // kernel benchmarks at ~3 per upstream docs), so this must never
        // block SessionStart.
        if event == HookEvent::SessionStart {
            let active_codebase_memory: Vec<&crate::config::CodebaseMemory> = config
                .features
                .as_ref()
                .map(|f| f.codebase_memory.as_slice())
                .unwrap_or_default()
                .iter()
                .filter(|cm| cm.when.iter().any(|t| active.tags.contains(t)))
                .collect();
            // Same "at most one active" rule as
            // `resolve_codebase_memory_entries` (every entry targets the same
            // project/cwd and the same MCP server name, so ambiguity can't be
            // resolved) — fail-soft here (log + skip) rather than propagate,
            // since this is a best-effort side effect, not manifest building.
            match active_codebase_memory.as_slice() {
                [] => {}
                [cm] => {
                    if let Ok((project_root, state_dir)) =
                        crate::mcp::resolve::codebase_memory_paths()
                    {
                        trigger_codebase_memory_index(&project_root, cm, &state_dir);
                    }
                }
                _ => {
                    tracing::debug!(
                        "codebase_memory: multiple entries active simultaneously, \
                         not triggering SessionStart index"
                    );
                }
            }
        }

        let config_dir = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;

        // Recall query: the sorted active tags. Store content: the llmenv context
        // chunk (tags/bundles/project). `active.tags` is a BTreeSet, so this
        // iteration order is already sorted ascending — no separate sort needed.
        let tags = active.tags.iter().cloned().collect::<Vec<_>>();
        let bundles = recall_bundle_names(&active);
        // Build per-tag and per-bundle recall queries. Validation rejects query
        // injection; these are the single sources of the tag/bundle→keyword encoding.
        let tag_queries = tag_recall_queries(&tags)?;
        let bundle_queries = bundle_recall_queries(&bundles)?;
        let query = tags.join(", ");
        let mut chunk = crate::icm::generate_context_chunk(&active, &bundles);

        // Apply default type/importance markers from config (R1, R3) when no explicit
        // marker is present in the generated chunk.
        chunk = apply_memory_config_defaults(chunk, &config, &active);
        // #317: folded into the chunk the SessionEnd store already sends,
        // rather than issuing a second `icm_memory_store` — one store per
        // session end keeps the memory readable and halves the round trips.
        if stores_session_metrics(event)
            && let Ok(state_dir) = crate::paths::state_dir()
            && let Some(summary) = crate::hook_run::slippage::session_metrics_summary(
                config.features.as_ref().and_then(|f| f.slippage.as_ref()),
                claude_session_id,
                &state_dir,
            )
        {
            chunk.push_str("\n\n");
            chunk.push_str(&summary);
        }

        // Reuse MCP HTTP client across events: the memory backend URL doesn't
        // change mid-session, so the reqwest Client (connection pool, TLS state,
        // DNS cache) is only built once. Cloning the cached McpHttpClient is
        // cheap — reqwest::Client is internally Arc, and the MCP session_id is
        // shared via Arc so re-initialization is also avoided.
        static MCP_CLIENT_CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();
        let resolved_client =
            resolve_memory_client(&config, config_dir, &active, event, &MCP_CLIENT_CACHE);
        let wakeup_max_tokens = resolved_client.as_ref().and_then(|r| r.wakeup_max_tokens);
        let client = resolved_client.map(|r| r.client);
        let state_path = Some(state::state_path());
        let ctx = build_scope_context(
            &active,
            tags,
            bundles,
            &env.cwd,
            adapter_name,
            claude_code_version,
        );

        // Dedup: skip the Store action when the context chunk hasn't changed
        // since the last SessionEnd (R3). Avoids redundant ICM writes when
        // hooks re-run. Only takes the early-return fast path when
        // session-log has no interest in SessionEnd; otherwise falls through
        // so `run_session_log` still runs below and only the redundant Store
        // action + dedup-snapshot rewrite are skipped, not the log — mirrors
        // the #864/#231 fix for the same early-return-drops-logging bug class.
        // Resolved once here (rather than again at the write-back below) since
        // it's the same target path for both the read-check and the rewrite.
        let session_end_dedup_path = if event == HookEvent::SessionEnd {
            Some(crate::paths::state_dir()?.join(crate::paths::HOOK_STORE_CHUNK))
        } else {
            None
        };
        let session_end_unchanged = if let Some(dedup_path) = &session_end_dedup_path {
            let is_unchanged = std::fs::read_to_string(dedup_path)
                .inspect_err(|e| {
                    tracing::warn!("failed to read dedup cache {}: {e}", dedup_path.display())
                })
                .ok()
                .is_some_and(|prev| prev == chunk);
            if is_unchanged {
                debug!("chunk unchanged since last store, skipping");
                if !log_cfg.any_sink_enabled() {
                    emit_trace_timing(t0, t_config, Some(t_scope), None, None);
                    return Ok(String::new());
                }
            }
            is_unchanged
        } else {
            false
        };

        // Reusable current-thread runtime: lifecycle hooks run on the agent's hot
        // path (session start + every prompt turn) and only need to `block_on` a
        // short sequence of HTTP round-trips. A multi-threaded runtime would spin up
        // a worker thread pool — pure overhead for this single sequential await. (#186)
        // Shared via OnceLock so the ~3ms builder overhead is paid once per session.
        static RUNTIME: OnceLock<std::io::Result<tokio::runtime::Runtime>> = OnceLock::new();
        let rt = RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        });
        let rt = match rt {
            Ok(rt) => rt,
            Err(e) => return Err(anyhow::anyhow!("failed to build tokio runtime: {e}")),
        };
        let session_log = SessionLogCall {
            log_cfg: &log_cfg,
            client: client.as_ref(),
            claude_session_id,
            ctx: &ctx,
            state_path: state_path.as_deref(),
        };
        let t_chunk = std::time::Instant::now();
        let out = rt.block_on(async {
            let mut out = String::new();
            // #866: skip the Store action (and its dedup-snapshot rewrite,
            // below) when SessionEnd's chunk is unchanged — but still run the
            // rest of this block (session-log capture) unconditionally.
            // `session_end_unchanged` is only ever `true` for `SessionEnd`
            // (see its computation above), so it alone is the store-skip
            // condition; no need to re-check the event here too.
            if let Some(client) = &client
                && !session_end_unchanged
            {
                let actions = dispatch(event, &tag_queries, &bundle_queries, wakeup_max_tokens);
                out = run_memory_actions(client, actions, &query, &chunk).await?;

                // PostSession: run reflective consolidation (R5) in a detached
                // child process so the hook returns immediately instead of
                // blocking on MCP. The result is fire-and-forget — PostSession is
                // the final event, so no caller needs its output.
                if is_post_session_consolidation_event(event) {
                    drop(post_session_consolidation());
                }

                // PostToolUse WebFetch/WebSearch: auto-store fetched content in ICM
                // with fast-falloff memory (topic: web-fetch, importance: low) so it
                // survives session compactions but decays quickly. (#579)
                if event == HookEvent::PostToolUse {
                    // Detached: process-group-detached and outlives us regardless.
                    let _detached_child = handle_web_fetch_post_tool_use(stdin_payload);
                }
            }
            run_session_log(event, &session_log, stdin_payload).await;

            // Update dedup snapshot *after* the store succeeds (R3). Writing before
            // the store call means a transient MCP failure leaves the snapshot ahead
            // of reality — the next SessionEnd sees the chunk as unchanged and skips
            // the store, permanently losing the memory. (#594 code review)
            if let Some(dedup_path) = &session_end_dedup_path
                && !session_end_unchanged
            {
                crate::paths::write_owner_only_atomic(dedup_path, chunk.as_bytes())?;
            }

            // #231: append the task-tracker Stop reminder. Only reached here when
            // session-log also wants Stop (the log_cfg.any_sink_enabled() case
            // above already short-circuited before this point otherwise) — so
            // this never displaces run_session_log, it just adds to `out`.
            if event == HookEvent::Stop && task_tracker_enabled {
                let state_dir = crate::paths::state_dir()?;
                let reminder = resolve_stop_reminder(&state_dir, claude_session_id, &config);
                if !reminder.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&reminder);
                }
            }

            // #864/#985: append the PreToolUse decision text (read-once or the
            // task-tool redirect). Only reached here when session-log also wants
            // PreToolUse at Debug level (the early-return above already short-
            // circuited otherwise) — so this never displaces run_session_log, it
            // just adds to `out` (a deny replaces it via append_read_once_result).
            if let Some(text) = &pre_tool_text
                && !text.is_empty()
            {
                append_read_once_result(&mut out, text);
            }
            // No emptiness check: `turn_text` is `None` rather than
            // `Some("")` when the layer produces nothing, so testing it again
            // here is dead — and a dead condition is one a future edit can
            // silently invert.
            if let Some(text) = &turn_text {
                append_read_once_result(&mut out, text);
            }

            Ok::<String, anyhow::Error>(out)
        })?;
        let t_end = std::time::Instant::now();
        emit_trace_timing(t0, t_config, Some(t_scope), Some(t_chunk), Some(t_end));
        Ok(out)
    })();

    match pipeline_result {
        Ok(out) => Ok(out),
        Err(e) => {
            // #867: an already-computed PreToolUse decision (read-once or the
            // #985 task-tool redirect) must not be lost to an unrelated pipeline
            // error — recover it instead of letting `?` propagate the error past
            // the point where it was decided. Still surfaced via the same
            // `eprintln!` convention `run()`'s Err arm uses (pre-pr-review
            // finding: a bare `warn!` is invisible with the default `RUST_LOG`,
            // silently hiding the diagnostic even though the result is preserved).
            if let Some(text) = pre_tool_text {
                eprintln!("llmenv: memory {event} skipped: {e}");
                warn!(
                    error = %e,
                    "hook-run: pipeline failed after a PreToolUse decision was \
                     already computed; returning it instead of silently dropping it"
                );
                // #1128: this is also a return point, though in practice the
                // memory-client/network failures this branch was written for
                // (#867) are caught fail-soft inside the pipeline itself and
                // never reach here — this covers the rarer errors that do
                // (e.g. a tag/bundle validation failure). t_scope/t_chunk/
                // t_end are lost along with the closure that computed them.
                emit_trace_timing(t0, t_config, None, None, None);
                Ok(text)
            } else {
                Err(e)
            }
        }
    }
}

/// Run one event's ordered memory actions and concatenate their text output.
///
/// TurnStart fans out to a project-scoped recall plus one per active tag and
/// bundle. When the same memory is stored under several of those keywords it
/// comes back from more than one recall, so the naive concatenation injects the
/// identical block two or three times — pure context/token cost with no added
/// information. Exact-duplicate action outputs are dropped (order preserved,
/// first wins); only byte-identical blocks are removed, so no unique recall is
/// ever lost.
async fn run_memory_actions(
    client: &McpHttpClient,
    actions: Vec<Action>,
    query: &str,
    chunk: &str,
) -> anyhow::Result<String> {
    let mut results: Vec<(bool, String)> = Vec::with_capacity(actions.len());
    for action in actions {
        let is_recall = matches!(
            action,
            Action::Recall | Action::RecallTag(_) | Action::RecallBundle(_)
        );
        let text = action.run(client, query, chunk).await?;
        results.push((is_recall, text));
    }
    let (kept, stats) = dedup_and_count_action_results(results);
    if stats.recall_entries > 0 || stats.recall_dropped > 0 {
        emit_context_trace(&stats, &kept);
    }
    Ok(kept.join("\n\n"))
}

/// Recall-specific counters produced alongside [`dedup_and_count_action_results`]'s
/// dedup pass, for [`emit_context_trace`] (#1261).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RecallStats {
    /// Recall-type actions (`Recall`/`RecallTag`/`RecallBundle`) whose
    /// response was non-empty after `strip_advisory`.
    recall_entries: usize,
    /// Total byte length of those non-empty responses.
    recall_bytes: usize,
    /// Recall-type actions whose response was dropped — either empty after
    /// `strip_advisory` (advisory-only noise), or an exact duplicate of an
    /// already-kept action's text.
    recall_dropped: usize,
}

/// Pure core of [`run_memory_actions`]'s dedup pass, split out so it's
/// testable without a live/mocked MCP client (#1261): given each action's
/// `(is_recall, text)` result, in dispatch order, drop empty and
/// exact-duplicate texts (first occurrence wins) and tally [`RecallStats`].
///
/// `dispatch` never mixes recall actions with `WakeUp`/`Store` in the same
/// batch (see its own doc comment), so `is_recall` is uniform across one
/// call in practice — checked per-entry anyway so the counters stay correct
/// if that ever changes, rather than assuming it from the batch's first
/// element.
fn dedup_and_count_action_results(results: Vec<(bool, String)>) -> (Vec<String>, RecallStats) {
    let mut kept: Vec<String> = Vec::new();
    let mut stats = RecallStats::default();
    for (is_recall, text) in results {
        if is_recall && !text.is_empty() {
            stats.recall_entries += 1;
            stats.recall_bytes += text.len();
        }
        if text.is_empty() || kept.contains(&text) {
            if is_recall {
                stats.recall_dropped += 1;
            }
            continue;
        }
        kept.push(text);
    }
    (kept, stats)
}

/// Emit `[LLMENV_CONTEXT] recall_entries=N recall_bytes=N injected_entries=N
/// injected_bytes=N advisory_stripped=N` to stderr when `LLMENV_TRACE_TIMING`
/// is set (#1261) — the same env var that already gates hook-run's other
/// stderr telemetry.
///
/// Granularity is per recall-type *action* (one project-scoped `Recall`, one
/// `RecallTag` per active tag, one `RecallBundle` per active bundle), not
/// per individual memory record within an action's response: parsing ICM's
/// recall-response text format to count records would couple this client to
/// a format owned by a separate system (see `crate::consolidation`'s own,
/// narrower parser, which only handles the non-compact format one specific
/// caller uses). `advisory_stripped` covers both ways a recalled action's
/// text ends up not injected — the whole response was advisory-only noise
/// (empty after `strip_advisory`), or it exactly duplicated an already-kept
/// action's text — rather than only the strictly-advisory case, since both
/// are "recalled but not injected" from an external observer's perspective.
fn emit_context_trace(stats: &RecallStats, kept: &[String]) {
    if std::env::var_os("LLMENV_TRACE_TIMING").is_none() {
        return;
    }
    let injected_entries = kept.len();
    let injected_bytes: usize = kept.iter().map(String::len).sum();
    eprintln!(
        "[LLMENV_CONTEXT] recall_entries={} recall_bytes={} injected_entries={injected_entries} \
         injected_bytes={injected_bytes} advisory_stripped={}",
        stats.recall_entries, stats.recall_bytes, stats.recall_dropped
    );
}

/// Borrowed inputs `run_session_log` needs, grouped to keep the function under
/// the project's positional-parameter limit.
struct SessionLogCall<'a> {
    log_cfg: &'a SessionLog,
    client: Option<&'a McpHttpClient>,
    claude_session_id: Option<&'a str>,
    ctx: &'a ScopeContext,
    state_path: Option<&'a std::path::Path>,
}

/// Dispatch the event's session-log handling: baseline lifecycle/scope events
/// for `SessionStart`/`SessionEnd`, or the per-hook capture event for every
/// other mapped event when any sink is enabled. No-op for unmapped events or
/// when no sink cares about this event's level.
async fn run_session_log(
    event: HookEvent,
    call: &SessionLogCall<'_>,
    stdin_payload: &serde_json::Value,
) {
    if matches!(event, HookEvent::SessionStart | HookEvent::SessionEnd) {
        handle_session_log(
            event,
            call.log_cfg,
            call.client,
            call.claude_session_id,
            call.ctx,
            call.state_path,
        )
        .await;
        return;
    }
    let Some((kind, role)) = event_to_log_kind(event) else {
        return;
    };
    let level = kind.log_level();
    if !call.log_cfg.any_sink_wants(level) {
        return;
    }
    let session_id = match call.claude_session_id {
        Some(csid) => {
            // Per-event: no verification, matching this path's pre-#1090
            // cost — see ensure_transcript_session's `verify` doc.
            ensure_transcript_session(
                call.log_cfg,
                call.client,
                csid,
                call.ctx,
                call.state_path,
                false,
            )
            .await
        }
        None => {
            debug!("event captured without claude_session_id — transcript record skipped");
            None
        }
    };
    let (tool_name, content) = event_content(event, stdin_payload);
    let trace_fields = if level == LogLevel::Trace {
        let mut tf = serde_json::json!({});
        if let Some(stdout) = stdin_payload.get("stdout").and_then(|v| v.as_str()) {
            tf["hook_stdout"] = serde_json::Value::String(stdout.to_string());
        }
        if let Some(stderr) = stdin_payload.get("stderr").and_then(|v| v.as_str()) {
            tf["hook_stderr"] = serde_json::Value::String(stderr.to_string());
        }
        if let Some(exit) = stdin_payload.get("exit_code") {
            tf["hook_exit_code"] = exit.clone();
        }
        Some(tf)
    } else {
        None
    };
    let mut ev = agent_session_event(kind, role, tool_name, content, serde_json::json!({}));
    ev.trace_fields = trace_fields;
    emit_session_log(ev, call.log_cfg, session_id.as_deref());
}

/// Build the active-scope context a session's lifecycle/scope-header events
/// carry. `tags`/`bundles` are the already-sorted/deduplicated sets `run_inner`
/// computed; the project name comes from the first project-kind active scope.
fn build_scope_context(
    active: &crate::scope::ActiveScopes,
    tags: Vec<String>,
    bundles: Vec<String>,
    cwd: &str,
    adapter_name: &str,
    claude_code_version: &str,
) -> ScopeContext {
    let project = active
        .scopes
        .iter()
        .find(|s| s.kind == "project")
        .and_then(|s| s.name.clone());
    ScopeContext {
        tags,
        bundles,
        project,
        cwd: cwd.to_string(),
        adapter: adapter_name.to_string(),
        llmenv_version: env!("CARGO_PKG_VERSION").to_string(),
        claude_code_version: claude_code_version.to_string(),
    }
}

/// Emit the baseline session-log events for `event`: `SessionStart` creates or
/// reuses the correlated transcript session, then emits `lifecycle_start` and
/// the scope-header `scope` event; `SessionEnd` emits `lifecycle_end` against
/// the previously-correlated session. No-op when both sinks are disabled, or
/// for any event other than session start/end. Fully fail-soft. Returns the
/// transcript session id this call resolved/used, if any (mainly for tests).
async fn handle_session_log(
    event: HookEvent,
    cfg: &SessionLog,
    client: Option<&McpHttpClient>,
    claude_session_id: Option<&str>,
    ctx: &ScopeContext,
    state_path: Option<&std::path::Path>,
) -> Option<String> {
    if !cfg.any_sink_enabled() {
        return None;
    }
    let session_id = match (event, claude_session_id) {
        (HookEvent::SessionStart, Some(csid)) => {
            // Best-effort reaping before any session-log activity.
            if let Some(days) = cfg.transcript.as_ref().and_then(|t| t.retention_days) {
                let log_path = cfg.file_path().map(std::path::PathBuf::from).or_else(|| {
                    crate::session_log::default_file_path()
                        .inspect_err(|e| {
                            tracing::debug!("session_log reaper: cannot resolve default path: {e}")
                        })
                        .ok()
                });
                if let Some(p) = log_path.as_ref() {
                    crate::session_log::reap_session_log(p, days);
                }
            }
            // SessionStart: once per launch, worth revalidating (#1090).
            ensure_transcript_session(cfg, client, csid, ctx, state_path, true).await
        }
        (_, Some(csid)) => state_path.and_then(|p| state::lookup_session_at(p, csid)),
        (_, None) => None,
    };
    let lifecycle_kind = match event {
        HookEvent::SessionStart => EventKind::LifecycleStart,
        HookEvent::SessionEnd => EventKind::LifecycleEnd,
        _ => return session_id,
    };
    emit_session_log(
        lifecycle_session_event(lifecycle_kind, &event.to_string()),
        cfg,
        session_id.as_deref(),
    );
    if event == HookEvent::SessionStart {
        emit_session_log(scope_session_event(ctx), cfg, session_id.as_deref());
    }
    session_id
}

/// Reuse a previously-recorded transcript session for `csid`, or — when
/// `cfg.transcript` and a client is available — start a new one and persist
/// the correlation. Returns `None` when transcript logging is unavailable and
/// nothing was recorded before.
///
/// `verify`: whether to revalidate a cached id against ICM before trusting it
/// (#1090) rather than treating its presence alone as proof of liveness — the
/// #1085 failure mode (a wrong cached value trusted forever because the only
/// check that would notice consulted the record itself). Revalidation costs a
/// full-transcript fetch (`icm_transcript_show` has no cheap existence-only
/// form), so only the `SessionStart` caller — once per launch — passes
/// `true`; the per-event caller in `run_session_log` passes `false` to avoid
/// turning every logged tool call into an ICM round trip that grows with the
/// transcript itself.
async fn ensure_transcript_session(
    cfg: &SessionLog,
    client: Option<&McpHttpClient>,
    csid: &str,
    ctx: &ScopeContext,
    state_path: Option<&std::path::Path>,
    verify: bool,
) -> Option<String> {
    let path = state_path?;
    if let Some(existing) = state::lookup_session_at(path, csid) {
        if !verify {
            return Some(existing);
        }
        match client {
            Some(client) => match transcript_dispatch::verify_session(client, &existing).await {
                Ok(()) => return Some(existing),
                Err(e) => {
                    error!(
                        error = %e,
                        session_id = %existing,
                        "cached ICM transcript session id failed verification, re-establishing"
                    );
                }
            },
            None => return Some(existing),
        }
    }
    let (true, Some(client)) = (cfg.transcript_wants(LogLevel::Info), client) else {
        return None;
    };
    let metadata = scope_metadata_json(ctx);
    match transcript_dispatch::start_session(
        client,
        &ctx.adapter,
        ctx.project.as_deref(),
        &metadata,
    )
    .await
    {
        Ok(id) => {
            if let Err(e) = state::record_session_at(path, csid, &id) {
                error!(error = %e, "failed to persist transcript session correlation");
            }
            Some(id)
        }
        Err(e) => {
            error!(error = %e, "failed to start ICM transcript session");
            None
        }
    }
}

/// Append `ev` to the configured sinks: the JSONL file (if enabled and
/// `ev.log_level() <= file.level`, written synchronously) and, for
/// agent-session-scoped events, the ICM transcript (if enabled and
/// `ev.log_level() <= transcript.level` — dispatched via a detached child, see
/// `session_log::detached`, so this never blocks on the network). Fail-soft.
fn emit_session_log(ev: SessionLogEvent, cfg: &SessionLog, session_id: Option<&str>) {
    let max = cfg.max_content_bytes.unwrap_or(16_384);
    let ev = ev.truncated(max);
    let level = ev.log_level();
    if cfg.file_wants(level) {
        let path = cfg.file_path().map(std::path::PathBuf::from).or_else(|| {
            crate::session_log::default_file_path()
                .inspect_err(|e| debug!("session_log: file sink disabled, no path resolved: {e}"))
                .ok()
        });
        if let Some(p) = path {
            crate::session_log::FileSink::new(p).append(&ev.to_jsonl());
        }
    }
    if cfg.transcript_wants(level)
        && ev.scope == EventScope::AgentSession
        && let Some(sid) = session_id
    {
        // Detached: process-group-detached and outlives us regardless.
        let _detached_child = crate::session_log::detached::spawn_record(sid, &ev);
    }
}

/// Shared defaults for every agent-session-scoped `SessionLogEvent`: current
/// timestamp, `AgentSession` scope, no tokens/level. Callers supply only
/// what varies (#509 item 3).
fn agent_session_event(
    kind: EventKind,
    role: &str,
    tool_name: Option<String>,
    content: String,
    fields: serde_json::Value,
) -> SessionLogEvent {
    SessionLogEvent {
        ts: now_rfc3339(),
        kind,
        scope: EventScope::AgentSession,
        role: role.to_string(),
        tool_name,
        tokens: None,
        level: None,
        content,
        fields,
        trace_fields: None,
    }
}

fn lifecycle_session_event(kind: EventKind, content: &str) -> SessionLogEvent {
    agent_session_event(
        kind,
        "system",
        None,
        content.to_string(),
        serde_json::json!({}),
    )
}

fn scope_session_event(ctx: &ScopeContext) -> SessionLogEvent {
    agent_session_event(
        EventKind::Scope,
        "system",
        None,
        scope_header_content(ctx),
        scope_metadata_json(ctx),
    )
}

/// Bundle names for recall keywords and the stored context chunk: every active
/// scope's `enable_bundles`, deduplicated and sorted, minus anything any scope
/// disables. `disable_bundles` wins here too (#1125) — a project that opts out
/// of a bundle must not have it named in queries sent to the memory backend or
/// in the context chunk stored there. Tag-fired bundles are deliberately
/// excluded: this list is the *explicitly requested* set, which is what makes a
/// useful recall keyword.
fn recall_bundle_names(active: &crate::scope::ActiveScopes) -> Vec<String> {
    let disabled = crate::cli::marker_disabled_bundle_names(active);
    active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter())
        .filter(|b| !disabled.contains(*b))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// What memory-backend resolution found for the active scope.
///
/// Replaces the `Option<String>` that collapsed five distinguishable states
/// into `None` (#1131/#1132/#1140): a caller could not tell a project that
/// simply declares no memory from one whose only memory-carrying bundle is
/// suppressed by `disable_bundles`, or one whose declared entry is simply
/// gated on a tag that isn't active right now. The fifth state — a failed
/// bundle merge — is an `Err` from [`memory_url`] rather than a variant here,
/// because a backend may well be configured and merely unparseable: that is a
/// failure, not an absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryEndpoint {
    /// The memory backend resolved to this HTTP URL, carrying the active
    /// `features.memory` entry's configured `wakeup_max_tokens` (#1216,
    /// `None` if unset).
    Active {
        url: String,
        wakeup_max_tokens: Option<u32>,
    },
    /// No bundle fired for the active scopes and no top-level `features.memory`
    /// entry matched — nothing could have supplied a backend.
    NoBundlesFired,
    /// Bundles fired, but neither they nor the top-level config declare a
    /// `features.memory` entry active for these tags. `skipped_bundles` names
    /// firing bundles that `build_bundle_refs` dropped — for having no content
    /// directory, or for a rejected/unsafe name — so their `bundle.yaml` was
    /// never read (#1133/#1142).
    NotDeclared { skipped_bundles: Vec<String> },
    /// `features.memory` is supplied only by these bundles, which the active
    /// scopes suppress via `disable_bundles` (#194).
    SuppressedByDisableBundles(Vec<String>),
    /// A top-level or firing-bundle `features.memory` entry exists for these
    /// `server_host`s, but none of their `when` tags intersect the active
    /// scope — distinct from [`Self::NoBundlesFired`] (nothing declared at
    /// all) and [`Self::NotDeclared`] (declared by a bundle whose content
    /// never loaded). `resolve_mcps`'s `0 => {}` arm drops a tag-inactive
    /// entry silently; this variant is how `classify_missing_memory`
    /// recovers that information on the failure path (#1140).
    TagInactive { server_hosts: Vec<String> },
}

impl MemoryEndpoint {
    /// The resolved URL, or an error naming why no backend is active.
    ///
    /// # Errors
    /// Every non-[`MemoryEndpoint::Active`] variant, rendered as the
    /// user-facing reason it is inactive.
    pub(crate) fn into_url(self) -> anyhow::Result<String> {
        const PREFIX: &str = "no memory backend active for this scope";
        match self {
            Self::Active { url, .. } => Ok(url),
            Self::NoBundlesFired => Err(anyhow::anyhow!(
                "{PREFIX}: no bundles fired and config.yaml declares no features.memory"
            )),
            Self::NotDeclared { skipped_bundles } if skipped_bundles.is_empty() => {
                Err(anyhow::anyhow!(
                    "{PREFIX}: no active bundle or config.yaml declares features.memory"
                ))
            }
            Self::NotDeclared { skipped_bundles } => Err(anyhow::anyhow!(
                "{PREFIX}: bundle(s) {} fired but were skipped while loading bundle \
                 content — either no content directory under the config dir's \
                 bundles/, or the bundle name was rejected (e.g. a traversal/absolute \
                 path) — so any features.memory they declare was never loaded",
                skipped_bundles.join(", ")
            )),
            Self::SuppressedByDisableBundles(names) => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory is supplied only by bundle(s) {}, which \
                 this project turns off via disable_bundles",
                names.join(", ")
            )),
            Self::TagInactive { server_hosts } => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory declares server_host(s) {}, but none of \
                 their `when` tags are in the active scope",
                server_hosts.join(", ")
            )),
        }
    }

    /// The active entry's configured wake-up token budget (#1216). `None`
    /// for every non-[`MemoryEndpoint::Active`] variant, and for `Active`
    /// itself when `features.memory[].wakeup_max_tokens` is unset.
    fn wakeup_max_tokens(&self) -> Option<u32> {
        match self {
            Self::Active {
                wakeup_max_tokens, ..
            } => *wakeup_max_tokens,
            _ => None,
        }
    }

    /// Consume into `(url, wakeup_max_tokens)`, erroring exactly as
    /// [`Self::into_url`] — `into_url` itself keeps its existing signature
    /// since it has several other callers that don't need the token budget.
    fn into_url_and_wakeup_max_tokens(self) -> anyhow::Result<(String, Option<u32>)> {
        let wakeup_max_tokens = self.wakeup_max_tokens();
        Ok((self.into_url()?, wakeup_max_tokens))
    }
}

/// A resolved memory-backend client plus the active `features.memory`
/// entry's configured wake-up token budget (#1216) — the two travel
/// together since both come from the same resolved endpoint.
struct ResolvedMemoryClient {
    client: McpHttpClient,
    wakeup_max_tokens: Option<u32>,
}

/// Resolve (or reuse from `cache`) the MCP client for the active memory
/// backend, for lifecycle-hook events. Returns `Option`, never `Result`: no
/// cause of an unresolved backend — including a bundle-merge failure
/// (#1132) — is fatal to the hook event. Memory actions are simply skipped;
/// session logging (independent of the memory backend) still proceeds. A
/// caller that instead wrote `memory_url(...)?.into_url()` would propagate
/// `memory_url`'s own `Err` via that leading `?` before `.into_url()` ever
/// ran, silently reintroducing the abort this function exists to prevent
/// (#1139).
fn resolve_memory_client(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    event: impl std::fmt::Display,
    cache: &'static OnceLock<Mutex<HashMap<String, McpHttpClient>>>,
) -> Option<ResolvedMemoryClient> {
    let (url, wakeup_max_tokens) = match memory_url(config, config_dir, active)
        .and_then(MemoryEndpoint::into_url_and_wakeup_max_tokens)
    {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("llmenv: memory {event} skipped: {e}");
            return None;
        }
    };
    let clients = cache.get_or_init(|| Mutex::new(HashMap::new()));
    let mut clients = clients.lock().unwrap_or_else(|e| e.into_inner());
    let client = match clients.entry(url) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            match McpHttpClient::new(entry.key().clone(), HOOK_TIMEOUT) {
                Ok(client) => entry.insert(client).clone(),
                Err(e) => {
                    eprintln!("llmenv: memory {event} skipped: invalid memory backend URL: {e}");
                    return None;
                }
            }
        }
    };
    Some(ResolvedMemoryClient {
        client,
        wakeup_max_tokens,
    })
}

/// Find the resolved memory backend's HTTP URL for the active tags, or the
/// reason none resolved.
///
/// Mirrors the `build_manifest` merge strategy: top-level config memory is
/// combined with bundle-contributed memory entries so a daemon declared only
/// in a `bundle.yaml` is reachable from lifecycle hooks.
///
/// `pub(crate)`: also called by `session_log::detached::run_record`, the
/// detached transcript-record child, which re-resolves the same MCP endpoint
/// independently rather than receiving it as a (process-list-visible) CLI arg.
///
/// # Errors
/// A bundle merge failure (#1132) or an unresolvable MCP/memory declaration.
pub(crate) fn memory_url(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<MemoryEndpoint> {
    let top_memory = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();

    // Collect bundle-contributed memory and host entries. Bundle selection goes
    // through `cli::firing_bundles` — the same selector `build_manifest` uses —
    // so `disable_bundles` suppression can't drift between hook-run's live
    // resolution and the materialized manifest (#1125). The `tag_filter` must
    // stay `None`: it exists for the CLI's `--tag` flag, and narrowing endpoint
    // resolution by one tag would drop the memory backend for a live session.
    let firing = crate::cli::firing_bundles(&config.bundle, active, None);

    let bundle_refs = crate::cli::build_bundle_refs(config_dir, active, &firing);
    let (bundle_memory, bundle_host) = resolve_bundle_memory_host(config, &bundle_refs)?;

    let mut all_memory: Vec<crate::config::Memory> = top_memory
        .iter()
        .chain(bundle_memory.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_memory);

    // Merged host: bundle contributions first, top-level overwrites (same as build_manifest).
    let mut all_host = bundle_host;
    for (k, v) in &config.host {
        all_host.insert(k.clone(), v.clone());
    }

    // Full `active.tags` (not `non_project_tags()`) on purpose: this resolves
    // the memory backend for the *live* hook-run session, which is legitimately
    // project-aware — unlike `build_manifest`'s static host-cache render, which
    // must exclude project-only tags (#696/#979). Do not "align" this with
    // build_manifest's host_tags; that would break recall in project scopes.
    let resolved = resolve_mcps(&config.mcp, &all_memory, &all_host, &active.tags)
        .map_err(|e| annotate_resolve_error(e, config, config_dir, active))?;
    let matched = resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => {
            Some((url, m.wakeup_max_tokens))
        }
        _ => None,
    });
    Ok(match matched {
        Some((url, wakeup_max_tokens)) => MemoryEndpoint::Active {
            url,
            wakeup_max_tokens,
        },
        None => classify_missing_memory(
            config,
            config_dir,
            active,
            &firing,
            &bundle_refs,
            &all_memory,
        ),
    })
}

/// Explain why no memory endpoint resolved (#1131).
///
/// Only reached once resolution has already come up empty, so the extra
/// `bundle.yaml` reads it does to attribute a cause stay off the hot path that
/// every hook event takes.
///
/// `all_memory` is the same merged top-level + bundle-contributed list
/// `resolve_mcps` was called with. By construction every entry in it has a
/// `when` that doesn't intersect `active.tags`: an intersecting entry would
/// have either resolved (`MemoryEndpoint::Active`) or, if more than one
/// intersected, made `resolve_mcps` return `Err` before this function is ever
/// reached. So a non-empty `all_memory` here means "declared, but tag-inactive"
/// (#1140), not "declared and active."
///
/// Priority order (checked in this order because each is a strictly more
/// actionable cause than the next): `disable_bundles` suppression, then a
/// firing bundle `build_bundle_refs` couldn't load (a real misconfiguration —
/// its `features.memory`, if any, was never even read), then a declared but
/// tag-inactive entry (often intentional — e.g. a `when` scoped to a network
/// the user isn't on right now), then no bundles firing at all, then the
/// fully benign case of bundles that fired, loaded, and simply declare no
/// memory.
fn classify_missing_memory(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    firing: &[&crate::config::Bundle],
    bundle_refs: &[crate::merge::BundleRef],
    all_memory: &[crate::config::Memory],
) -> MemoryEndpoint {
    let suppressed = suppressed_memory_bundles(config, config_dir, active);
    if !suppressed.is_empty() {
        return MemoryEndpoint::SuppressedByDisableBundles(suppressed);
    }
    let loaded: std::collections::HashSet<&str> =
        bundle_refs.iter().map(|r| r.name.as_str()).collect();
    let skipped_bundles: Vec<String> = firing
        .iter()
        .map(|b| b.name.as_str())
        .filter(|n| !loaded.contains(n))
        .map(str::to_owned)
        .collect();
    if !skipped_bundles.is_empty() {
        return MemoryEndpoint::NotDeclared { skipped_bundles };
    }
    if !all_memory.is_empty() {
        return MemoryEndpoint::TagInactive {
            server_hosts: all_memory
                .iter()
                .map(|m| m.server_host.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
    }
    if firing.is_empty() {
        return MemoryEndpoint::NoBundlesFired;
    }
    MemoryEndpoint::NotDeclared {
        skipped_bundles: Vec::new(),
    }
}

/// Name `disable_bundles` in a resolution failure when that is what withdrew
/// the `host:` entry the memory block points at (#1131). Without it the user
/// sees a `server_host` they can read in their own `config.yaml` and nothing
/// connecting it to the bundle they turned off.
fn annotate_resolve_error(
    err: crate::mcp::resolve::ResolveError,
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Error {
    if let crate::mcp::resolve::ResolveError::MemoryUnknownServerHost(host) = &err {
        let suppliers: Vec<String> = suppressed_bundle_capabilities(config, config_dir, active)
            .into_iter()
            .filter(|(_, caps)| caps.host.contains_key(host))
            .map(|(name, _)| name)
            .collect();
        if !suppliers.is_empty() {
            return anyhow::anyhow!(
                "failed to resolve MCP servers: {err} — it is declared in bundle(s) \
                 {}, which this project turns off via disable_bundles",
                suppliers.join(", ")
            );
        }
    }
    anyhow::anyhow!("failed to resolve MCP servers: {err}")
}

/// `bundle.yaml` capabilities of every bundle the active scopes suppress via
/// `disable_bundles` (#194) but that would otherwise have fired, in config
/// declaration order.
///
/// Diagnostic-only, read on the failure path so llmenv can say *why* no
/// endpoint resolved rather than only *that* none did. A suppressed bundle
/// whose own `bundle.yaml` is missing or unreadable contributes nothing here —
/// a failed explanation must not replace the failure being explained.
///
/// `pub(crate)`: also called by `cli::doctor`, whose orphaned-memory check must
/// see the same suppressed declarations this diagnostic does.
fn suppressed_bundle_capabilities(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> Vec<(String, crate::config::Capabilities)> {
    let disabled = crate::cli::marker_disabled_bundle_names(active);
    if disabled.is_empty() {
        return Vec::new();
    }
    let manually_enabled = crate::cli::marker_enabled_bundle_names(active);
    let would_fire: Vec<&crate::config::Bundle> = config
        .bundle
        .iter()
        .filter(|b| disabled.contains(&b.name))
        .filter(|b| crate::cli::tag_or_marker_selected(b, active, &manually_enabled))
        .collect();
    crate::cli::build_bundle_refs(config_dir, active, &would_fire)
        .into_iter()
        .filter_map(|r| {
            crate::merge::read_bundle_yaml(&r.path, &r.name)
                .ok()
                .flatten()
                .map(|caps| (r.name, caps))
        })
        .collect()
}

/// Names of [`suppressed_bundle_capabilities`]' bundles that would supply a
/// tag-active `features.memory` entry if `disable_bundles` didn't suppress
/// them — the "would this disabled bundle have supplied memory" filter,
/// shared so `classify_missing_memory` and
/// `cli::doctor::memory_orphaned_by_disable_bundles` can't drift on it
/// (#1141). Tag-active, not merely present (#1140): a suppressed bundle whose
/// only `features.memory` entry is itself gated on an inactive tag wouldn't
/// have supplied memory even if re-enabled, so it must not be named as the
/// cause.
///
/// `pub(crate)`: called by `cli::doctor`, whose orphaned-memory check needs
/// the same filtered list this diagnostic does.
pub(crate) fn suppressed_memory_bundles(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> Vec<String> {
    suppressed_bundle_capabilities(config, config_dir, active)
        .into_iter()
        .filter(|(_, caps)| {
            caps.features.as_ref().is_some_and(|f| {
                f.memory
                    .iter()
                    .any(|m| crate::mcp::resolve::memory_is_tag_active(m, &active.tags))
            })
        })
        .map(|(name, _)| name)
        .collect()
}

/// Resolve the bundle-only memory/host slice for `bundle_refs` (#920).
///
/// Tries the disk-persisted cache first — written by `regenerate`/`export`
/// (`build_manifest` in `cli/mod.rs`) — keyed on `merge_signature`, which is
/// cheap to recompute here (reads only each firing bundle's `bundle.yaml`,
/// not the full merge). A hit skips the full `merge()` call entirely; a miss
/// (no artifact yet, or the signature changed because config or bundle
/// content changed since the last regenerate) falls back to a live merge.
///
/// Both failure modes are reported rather than swallowed (#1132). A signature
/// failure only costs the optimization, so it logs and falls through to the
/// live merge; a live-merge failure means a backend may well be configured and
/// unparseable, so it propagates — the lifecycle hook's fail-soft wrapper turns
/// it into `llmenv: memory <event> skipped: {e}`, which names the real cause
/// instead of the misleading "no memory backend active for this scope".
fn resolve_bundle_memory_host(
    config: &crate::config::Config,
    bundle_refs: &[crate::merge::BundleRef],
) -> anyhow::Result<(
    Vec<crate::config::Memory>,
    std::collections::BTreeMap<String, crate::config::HostEntry>,
)> {
    if bundle_refs.is_empty() {
        return Ok((Vec::new(), std::collections::BTreeMap::new()));
    }

    let disk_hit = crate::merge::merge_signature(&config.capabilities, &config.native, bundle_refs)
        .inspect_err(|e| {
            // `error!`, not `warn!` (#1139): the process's own `EnvFilter` is
            // ERROR-only by default, same as the three detached children this
            // diff fixes for the identical reason — a `warn!` here would be
            // silently dropped, same as theirs was.
            tracing::error!("failed to compute merge signature for cache lookup: {e}");
        })
        .ok()
        .and_then(|key| {
            let cache_root =
                std::path::PathBuf::from(crate::paths::expand_tilde(&config.cache.cache_dir));
            crate::materialize::merge_cache::read_if_matching(&cache_root, &key)
        });
    if let Some(hit) = disk_hit {
        return Ok(hit);
    }

    let merged = crate::merge::merge(&config.capabilities, &config.native, bundle_refs)
        .context("failed to merge bundle capabilities for memory-backend resolution")?;
    let mem = merged
        .capabilities
        .features
        .map(|f| f.memory)
        .unwrap_or_default();
    Ok((mem, merged.capabilities.host))
}

/// Apply default memory type/importance markers from the active memory config (R1, R3).
///
/// If the chunk already contains an `<!-- llmenv-type: -->` or
/// `<!-- llmenv-importance: -->` marker, the inline value takes precedence and
/// no default is appended. Otherwise the config's `default_type` /
/// `default_importance` are appended as markers at the end of the chunk.
///
/// ponytail: `type_importance` per-type overrides are not yet applied here —
/// they will be resolved when the Store action runs against the ICM backend.
fn apply_memory_config_defaults(
    mut chunk: String,
    config: &crate::config::Config,
    active: &crate::scope::ActiveScopes,
) -> String {
    let Some(mem) = config.features.as_ref().and_then(|f| {
        f.memory
            .iter()
            .find(|m| m.when.iter().any(|t| active.tags.contains(t)))
    }) else {
        return chunk;
    };

    if !chunk.contains("<!-- llmenv-type:")
        && let Some(ty) = &mem.default_type
    {
        chunk.push_str(&format!("\n<!-- llmenv-type: {} -->", ty.as_marker_str()));
    }

    if !chunk.contains("<!-- llmenv-importance:")
        && let Some(imp) = &mem.default_importance
    {
        chunk.push_str(&format!(
            "\n<!-- llmenv-importance: {} -->",
            imp.as_marker_str()
        ));
    }

    chunk
}

/// Validate a tag to prevent query injection. Tags must be alphanumeric with
/// hyphens and underscores only (same as bundle/scope naming).
fn validate_tag(tag: &str) -> anyhow::Result<()> {
    if tag.is_empty() {
        return Err(anyhow::anyhow!("empty tag in recall query"));
    }
    if !crate::scope::matcher::is_valid_tag_charset(tag) {
        return Err(anyhow::anyhow!(
            "tag '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            tag
        ));
    }
    Ok(())
}

/// Validate a bundle name to prevent query injection. Bundle names follow the
/// same character rules as tags: alphanumeric, hyphens, and underscores only.
fn validate_bundle(bundle: &str) -> anyhow::Result<()> {
    if bundle.is_empty() {
        return Err(anyhow::anyhow!("empty bundle name in recall query"));
    }
    if !crate::scope::matcher::is_valid_tag_charset(bundle) {
        return Err(anyhow::anyhow!(
            "bundle '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            bundle
        ));
    }
    Ok(())
}

/// Tool name constants for WebFetch and WebSearch tools.
const TOOL_NAME_WEBFETCH: &str = "WebFetch";
const TOOL_NAME_WEBSEARCH: &str = "WebSearch";

/// Build ICM memory store arguments for a WebFetch/WebSearch PostToolUse event.
/// Returns `None` if the payload is not a WebFetch/WebSearch tool result.
///
/// # Format
/// The stored memory carries topic `web-fetch` and importance `low` so it decays
/// quickly and can be bulk-cleared via `icm_memory_forget_topic("web-fetch")`.
#[must_use]
fn web_fetch_store_args(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let tool_name = payload["tool_name"].as_str()?;
    if tool_name != TOOL_NAME_WEBFETCH && tool_name != TOOL_NAME_WEBSEARCH {
        return None;
    }
    let is_search = tool_name == TOOL_NAME_WEBSEARCH;
    let source_field = if is_search { "query" } else { "url" };
    let source_value = payload["tool_input"][source_field]
        .as_str()
        .unwrap_or("unknown");
    let label = if is_search { "Query" } else { "URL" };
    let response = payload["tool_response"]
        .as_str()
        .map_or_else(|| json_or_empty(&payload["tool_response"]), String::from);
    let needs_indicator = response.chars().count() > 1000;
    let mut truncated: String = response.chars().take(1000).collect();
    if needs_indicator {
        truncated.push_str("... (truncated)");
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Some(json!({
        "content": format!(
            "{label}: {source_value}\nTool: {tool_name}\nFetched at (epoch): {timestamp}\nContent preview:\n{truncated}"
        ),
        "topic": "web-fetch",
        "importance": "low",
    }))
}

/// Handle PostToolUse for WebFetch/WebSearch by spawning a detached child
/// that stores the fetched content in ICM with fast-falloff memory
/// (importance: low, topic: web-fetch). The hook returns immediately instead of
/// blocking on the MCP round trip. Best-effort — failures are logged at debug
/// level and never propagated to the caller. The child's stderr goes to the
/// shared bounded log rather than `/dev/null` so its own failures are
/// diagnosable (#1133).
///
/// Returns the spawned [`std::process::Child`] purely so callers such as
/// tests can reap it (#1095) — production intentionally drops it unwaited,
/// identical to the previous behavior, since the child is
/// process-group-detached and outlives this process regardless.
fn handle_web_fetch_post_tool_use(payload: &serde_json::Value) -> Option<std::process::Child> {
    let args = web_fetch_store_args(payload)?;
    let Ok(payload_json) = serde_json::to_string(&args) else {
        tracing::debug!("icm-store: failed to serialize store args");
        return None;
    };
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("icm-store: cannot resolve current_exe for detached store");
        return None;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("icm-store")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null());
    redirect_stderr_to_detached_log(&mut cmd, detached_child_log_path);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::debug!("icm-store: failed to spawn detached store child");
        return None;
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        tracing::debug!("icm-store: failed to pipe args to detached child: {e}");
    }
    // Not waited on by the caller: the child is process-group-detached and
    // outlives us.
    Some(child)
}

/// Where codebase-memory-mcp's index cache lives for `cm`/`state_dir`: the
/// configured `index_path` override, or `state_dir/codebase-memory` by
/// default. Single source of truth for both the spawned child's
/// `CBM_CACHE_DIR` and the indexer's own diagnostic log (#1091), so they
/// can't drift apart.
fn codebase_memory_cache_dir(
    cm: &crate::config::CodebaseMemory,
    state_dir: &std::path::Path,
) -> std::path::PathBuf {
    cm.index_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state_dir.join("codebase-memory"))
}

/// Rotation bound shared by every size-bounded stderr log this module opens —
/// the indexer's diagnostic log and the detached hook children's shared log
/// (#1086/#1091 share the "size-bounded" shape and, previously, a
/// byte-for-byte identical constant under two names; merged under #1141).
/// Smaller than the mcp-proxy log: these children run often but write
/// nothing unless they fail, and indexing runs are far less frequent than
/// proxy restarts.
const BOUNDED_LOG_MAX_BYTES: u64 = 1 << 19; // 512 KiB

/// Path of the stderr log shared by llmenv's detached hook children —
/// `<state_dir>/detached-hook.log`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub(crate) fn detached_child_log_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::state_dir()?.join("detached-hook.log"))
}

/// Point `cmd`'s stderr at `log_path` as a size-bounded diagnostic log.
///
/// Sets the null baseline first, unconditionally (#1139): `Command`'s default
/// for an unset stdio is `Stdio::inherit()`, not discarded, so a caller that
/// only overrode stderr on the `Ok` branch would leave the child holding
/// whichever fd this process's own stderr happens to be on a log-open
/// failure — the exact hang/leak this redirect exists to prevent. If the log
/// can't be opened the child still runs with stderr discarded — a missing
/// diagnostic is a smaller problem than skipping the work.
///
/// `dir_mode` is forwarded to `open_bounded_log`, which does the 0700
/// hardening itself (#1196) — pass `LogDirMode::Inherit` when `log_path`'s
/// directory may be shared with a process running under a different uid
/// (e.g. a user-configured `index_path`). `context` names the caller in the
/// debug-level "log unavailable" message, since that message is shared across
/// callers with different failure consequences (#1141). No `max_bytes`
/// parameter: every caller bounds to the same [`BOUNDED_LOG_MAX_BYTES`] now
/// that the two call sites' previously-distinct constants turned out to be
/// identical (#1141) — a parameter every caller passes the same value for
/// isn't a real degree of freedom.
fn redirect_stderr_to_bounded_log(
    cmd: &mut std::process::Command,
    log_path: &std::path::Path,
    dir_mode: crate::mcp::proxy::LogDirMode,
    context: &str,
) {
    cmd.stderr(std::process::Stdio::null());
    match crate::mcp::proxy::open_bounded_log(log_path, BOUNDED_LOG_MAX_BYTES, dir_mode) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            tracing::debug!("{context}: log unavailable ({e:#}), stderr discarded");
        }
    }
}

/// Send a detached child's stderr to the shared bounded log instead of
/// discarding it (#1133, the same remedy as #1091).
///
/// `Stdio::null()` leaves such a child with no report channel whatsoever: its
/// own `tracing` events go to a fmt layer writing to that same null stderr, so
/// a failure is discarded twice over.
///
/// `log_path` resolves the log's location; every real caller passes
/// [`detached_child_log_path`]. Parameterized rather than calling it
/// directly so a test can inject a fixed tempdir path — this workspace
/// forbids `unsafe`, so a test can't safely override `state_dir()`'s
/// `LLMENV_STATE_DIR`/`HOME` env vars to control the real resolver instead.
pub(crate) fn redirect_stderr_to_detached_log(
    cmd: &mut std::process::Command,
    log_path: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) {
    match log_path() {
        // Always state_dir-rooted, so `LogDirMode::OwnerOnly` is safe.
        Ok(path) => redirect_stderr_to_bounded_log(
            cmd,
            &path,
            crate::mcp::proxy::LogDirMode::OwnerOnly,
            "detached child",
        ),
        Err(e) => {
            cmd.stderr(std::process::Stdio::null());
            tracing::debug!("detached child: cannot resolve log path ({e:#}), stderr discarded");
        }
    }
}

/// Builds the `codebase-memory-mcp cli index_repository` subprocess command
/// for `project_root`, without spawning it — kept separate so tests can
/// assert on the command shape without launching a real process (#365).
fn build_index_repository_command(
    project_root: &std::path::Path,
    cm: &crate::config::CodebaseMemory,
    state_dir: &std::path::Path,
) -> std::process::Command {
    // repo_path must become JSON text regardless (the CLI arg is a JSON
    // string), so this lossy step is unavoidable here — unlike the env vars
    // below, which can carry the raw OsStr straight through.
    let args_json =
        serde_json::json!({ "repo_path": project_root.display().to_string() }).to_string();
    let cache_dir = codebase_memory_cache_dir(cm, state_dir);
    let mut cmd = std::process::Command::new("codebase-memory-mcp");
    cmd.args(["cli", "index_repository", &args_json])
        .env("CBM_ALLOWED_ROOT", project_root.as_os_str())
        .env("CBM_CACHE_DIR", cache_dir.as_os_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

/// Fire-and-forget: registers `project_root` with codebase-memory-mcp's
/// index + background auto-watch (#365). Mirrors `post_session_consolidation`'s
/// detached-spawn pattern — indexing a large repo can take minutes (the
/// Linux kernel takes ~3, per upstream benchmarks), so this must never block
/// SessionStart. Best-effort: spawn failures (e.g. binary not installed —
/// `llmenv doctor` already flags that) are logged at debug level only.
///
/// The child's stderr is redirected to a bounded log rather than discarded
/// (#1091, same remedy as #1087's mcp-proxy stderr fix): a multi-minute
/// indexer that fails partway through used to leave nothing to diagnose —
/// only the spawn error was recorded, at a level the default `EnvFilter`
/// drops. If the log can't be opened, indexing still proceeds — a missing
/// diagnostic is a smaller problem than skipping indexing — but stderr then
/// falls back to discarded.
fn trigger_codebase_memory_index(
    project_root: &std::path::Path,
    cm: &crate::config::CodebaseMemory,
    state_dir: &std::path::Path,
) {
    let mut cmd = build_index_repository_command(project_root, cm, state_dir);
    let cache_dir = codebase_memory_cache_dir(cm, state_dir);
    let log_path = cache_dir.join("index.log");
    // Only the default cache dir (under llmenv's own state tree) gets
    // hardened to 0700. A user-configured `index_path` (#1196) can be shared
    // with a codebase-memory-mcp process running under a different uid —
    // forcing it to 0700 would silently break that sharing with an EACCES on
    // the next run.
    let dir_mode = if cm.index_path.is_none() {
        crate::mcp::proxy::LogDirMode::OwnerOnly
    } else {
        crate::mcp::proxy::LogDirMode::Inherit
    };
    redirect_stderr_to_bounded_log(
        &mut cmd,
        &log_path,
        dir_mode,
        "codebase-memory-mcp index_repository",
    );
    crate::mcp::proxy::detach_process_group(&mut cmd);
    if let Err(e) = cmd.spawn() {
        tracing::debug!("codebase-memory-mcp index_repository: failed to spawn: {e}");
    }
}

/// Whether `event` should trigger [`post_session_consolidation`] — only
/// `PostSession`, the final event of a session. Extracted as its own
/// directly-testable predicate (#1465): the call site sits inside
/// `run_inner`'s async, MCP-client-mocked block, too heavy a harness to
/// exercise just for this one routing decision.
fn is_post_session_consolidation_event(event: HookEvent) -> bool {
    event == HookEvent::PostSession
}

/// Spawn a detached child to run post-session consolidation. Best-effort
/// fire-and-forget — spawn failures are logged at debug level and the caller
/// never waits on the child. The child's stderr goes to the shared bounded log
/// rather than `/dev/null` so its own failures are diagnosable (#1133).
///
/// Returns the spawned [`std::process::Child`] purely so callers such as
/// tests can reap it (#1095) — production intentionally drops it unwaited,
/// identical to the previous behavior, since the child is
/// process-group-detached and outlives this process regardless.
fn post_session_consolidation() -> Option<std::process::Child> {
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("consolidation-run: cannot resolve current_exe");
        return None;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("consolidation-run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    redirect_stderr_to_detached_log(&mut cmd, detached_child_log_path);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            tracing::debug!("consolidation-run: failed to spawn detached child: {e}");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn append_pending_notice_joins_with_a_newline_when_text_is_non_empty() {
        let result = append_pending_notice(
            "existing memory context".to_string(),
            Some("config changed".to_string()),
        );
        assert_eq!(result, "existing memory context\nconfig changed");
    }

    #[test]
    fn append_pending_notice_has_no_leading_newline_when_text_is_empty() {
        let result = append_pending_notice(String::new(), Some("config changed".to_string()));
        assert_eq!(result, "config changed");
    }

    #[test]
    fn append_pending_notice_leaves_text_unchanged_when_there_is_no_notice() {
        let result = append_pending_notice("existing memory context".to_string(), None);
        assert_eq!(result, "existing memory context");
    }

    /// Every event `from_str` accepts. Kept as strings so a new variant that
    /// forgets to round-trip is a test failure rather than a silent gap.
    const ALL_HOOK_EVENTS: &[&str] = &[
        "session_start",
        "turn_start",
        "session_end",
        "post_session",
        "user_prompt_submit",
        "pre_tool_use",
        "post_tool_use",
        "notification",
        "stop",
        "subagent_stop",
        "pre_compact",
    ];

    #[test]
    fn all_hook_events_covers_every_variant() {
        for name in ALL_HOOK_EVENTS {
            let parsed = HookEvent::from_str(name).unwrap();
            assert_eq!(&parsed.to_string(), name, "{name} does not round-trip");
        }
        // `HookEvent` derives no variant count, so this guards the list
        // against a variant added to the enum but not to `from_str`.
        assert_eq!(ALL_HOOK_EVENTS.len(), 11);
    }

    // The gate is fed `AgentAdapter::name` (hyphenated), not `engine_id`
    // (underscored). Driving it from the real registry means a mismatch shows
    // up here instead of silently disabling the check.
    #[test]
    fn only_claude_code_session_start_triggers_the_drift_check() {
        let mut triggered = Vec::new();
        for adapter in crate::adapter::registered_adapters() {
            for name in ALL_HOOK_EVENTS {
                let event = HookEvent::from_str(name).unwrap();
                if should_check_stale(event, adapter.name()) {
                    triggered.push(format!("{}/{name}", adapter.name()));
                }
            }
        }
        assert_eq!(triggered, vec!["claude-code/session_start".to_string()]);
    }

    // #1331: opencode's shim blocks on `code === 2` alone, so a deny that
    // exits 0 there is silently allowed. Claude Code stays on exit 0 — it
    // honours the envelope, and this keeps a working path unchanged.

    // #231/#864: returning early when a sink still wants the event drops its
    // log line, and both arms return the same text — so nothing but a direct
    // test of the condition can tell the two apart.
    #[test]
    fn short_circuit_is_refused_while_a_sink_still_wants_the_event() {
        let quiet = crate::config::SessionLog {
            file: None,
            transcript: None,
            ..Default::default()
        };
        assert!(
            can_short_circuit(HookEvent::UserPromptSubmit, &quiet),
            "no sink wants it, so the decision can return immediately"
        );
        assert!(can_short_circuit(HookEvent::PreToolUse, &quiet));

        let listening = crate::config::SessionLog::default();
        assert!(
            listening.any_sink_enabled(),
            "fixture must have a sink on, or this proves nothing"
        );
        assert!(
            !can_short_circuit(HookEvent::UserPromptSubmit, &listening),
            "a listening sink must be reached before returning"
        );
    }

    // #317: each layer fires on exactly one event. Inside `run_inner` these
    // sit behind payload reads and state-dir resolves, so a wrong event shows
    // up as "the feature quietly does nothing" rather than a failure.
    #[test]
    fn each_slippage_layer_fires_on_exactly_one_event() {
        let matches = |f: fn(HookEvent) -> bool| {
            ALL_HOOK_EVENTS
                .iter()
                .filter(|name| f(HookEvent::from_str(name).unwrap()))
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(matches(counts_tool_use), vec!["post_tool_use".to_string()]);
        assert_eq!(
            matches(carries_turn_digest),
            vec!["user_prompt_submit".to_string()]
        );
        assert_eq!(
            matches(stores_session_metrics),
            vec!["session_end".to_string()]
        );
    }

    // #317: the checklist is appended to whatever the Stop hook already had to
    // say, and neither piece may swallow the other.
    #[test]
    fn stop_reminder_joins_the_tracker_text_and_the_critique() {
        let state_dir = tempfile::tempdir().expect("test");
        let with_critique = crate::config::Config {
            features: Some(crate::config::Features {
                slippage: Some(crate::config::SlippageControl {
                    enabled: true,
                    self_critique: true,
                    ..Default::default()
                }),
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: false,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = resolve_stop_reminder(state_dir.path(), Some("s1"), &with_critique);
        assert!(text.contains("tests"), "the critique is present: {text:?}");

        let without = crate::config::Config {
            features: Some(crate::config::Features {
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: false,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let plain = resolve_stop_reminder(state_dir.path(), Some("s1"), &without);
        assert!(
            !plain.contains("tests"),
            "no critique when the layer is off: {plain:?}"
        );
        // The tracker's own text survives either way — appending must not
        // replace what was already there.
        assert!(
            text.starts_with(plain.as_str()) || plain.is_empty(),
            "the critique is appended, not substituted: {text:?} vs {plain:?}"
        );
    }

    // The separator only matters when the tracker actually said something, so
    // an empty-tracker fixture can't tell a correct join from a missing or
    // misplaced one. Seed a task first, then require both parts and a blank
    // line between them.
    #[test]
    fn stop_reminder_separates_tracker_text_from_the_critique() {
        let state_dir = tempfile::tempdir().expect("test");
        // The reminder only lists `wip` tasks belonging to the *current*
        // project, resolved from cwd — a fixture project string would be
        // filtered straight back out.
        let project = crate::task::project::current_tag().expect("test");
        crate::task::session::start_session(
            state_dir.path(),
            None,
            None,
            &project,
            crate::task::session::StartDecision::Auto,
        )
        .expect("test");
        let task = crate::task::add_task(
            state_dir.path(),
            "finish the parser",
            crate::task::ParentSpec::Auto,
            None,
            &project,
        )
        .expect("test");
        crate::task::start_task(state_dir.path(), &task.slug, false).expect("test");

        let config = crate::config::Config {
            features: Some(crate::config::Features {
                slippage: Some(crate::config::SlippageControl {
                    enabled: true,
                    self_critique: true,
                    ..Default::default()
                }),
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: false,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = resolve_stop_reminder(state_dir.path(), Some("s1"), &config);
        let tracker_only = crate::task::stop_hook_reminder(state_dir.path());
        assert!(
            !tracker_only.is_empty(),
            "fixture must produce tracker text, or this proves nothing"
        );
        assert!(
            text.contains(tracker_only.trim()),
            "tracker text kept: {text:?}"
        );
        assert!(text.contains("tests"), "critique kept: {text:?}");
        assert!(
            text.contains("\n\n"),
            "the two are separated by a blank line: {text:?}"
        );
        assert!(
            !text.starts_with('\n'),
            "no leading separator when the tracker already spoke: {text:?}"
        );
    }

    #[test]
    fn only_opencode_needs_the_exit_code_block_signal() {
        let mut by_exit_code = Vec::new();
        for adapter in crate::adapter::registered_adapters() {
            if blocks_by_exit_code(adapter.name()) {
                by_exit_code.push(adapter.name().to_string());
            }
        }
        assert_eq!(by_exit_code, vec!["opencode".to_string()]);
    }

    #[test]
    fn drift_check_gate_rejects_the_underscored_engine_id() {
        // `engine_id` is what `--engine` and config keys use; `name` is what
        // the gate actually receives. Asserting both directions keeps a future
        // refactor from swapping one for the other unnoticed.
        assert!(should_check_stale(HookEvent::SessionStart, "claude-code"));
        assert!(!should_check_stale(HookEvent::SessionStart, "claude_code"));
    }

    // #1128: the marker used to require reaching the full success path (only
    // 4 of 11 hook events ever got there); every early-return in run_inner
    // now emits it too, with whichever phases it actually reached. Each
    // field is present only when the corresponding Instant was reached.
    #[test]
    fn trace_timing_json_includes_only_reached_phases() {
        use std::collections::BTreeSet;
        let keys_of = |v: &serde_json::Value| -> BTreeSet<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };

        let t0 = std::time::Instant::now();
        let t_config = t0 + std::time::Duration::from_micros(10);

        let only_config = trace_timing_json(t0, t_config, None, None, None);
        assert_eq!(
            keys_of(&only_config),
            BTreeSet::from(["config_load_us".to_string()]),
            "an early return before scope eval must report only config_load_us"
        );

        let t_scope = t_config + std::time::Duration::from_micros(20);
        let through_scope = trace_timing_json(t0, t_config, Some(t_scope), None, None);
        assert_eq!(
            keys_of(&through_scope),
            BTreeSet::from(["config_load_us".to_string(), "scope_eval_us".to_string()]),
            "an early return after scope eval must add scope_eval_us"
        );

        let t_chunk = t_scope + std::time::Duration::from_micros(30);
        let t_end = t_chunk + std::time::Duration::from_micros(40);
        let full = trace_timing_json(t0, t_config, Some(t_scope), Some(t_chunk), Some(t_end));
        assert_eq!(
            keys_of(&full),
            BTreeSet::from([
                "config_load_us".to_string(),
                "scope_eval_us".to_string(),
                "prep_us".to_string(),
                "mcp_us".to_string(),
            ]),
            "the full success path must report all four phases"
        );
        assert_eq!(full["config_load_us"], 10);
        assert_eq!(full["scope_eval_us"], 20);
        assert_eq!(full["prep_us"], 30);
        assert_eq!(full["mcp_us"], 40);
    }

    proptest! {
        // #1128: trace_timing_json must never panic for any gap between
        // phases, including a zero gap (Instants captured in the same tick)
        // and a gap large enough to push the microsecond count past u64::MAX
        // (~585,000 years — saturating_duration_since + the try_from/
        // unwrap_or(u64::MAX) fallback must absorb it rather than panic).
        #[test]
        fn trace_timing_json_never_panics_for_arbitrary_gaps(
            config_gap_secs in 0u64..1_000_000_000_000,
            scope_gap_secs in 0u64..1_000_000_000_000,
            chunk_gap_secs in 0u64..1_000_000_000_000,
            end_gap_secs in 0u64..1_000_000_000_000,
        ) {
            let t0 = std::time::Instant::now();
            let t_config = t0 + std::time::Duration::from_secs(config_gap_secs);
            let t_scope = t_config + std::time::Duration::from_secs(scope_gap_secs);
            let t_chunk = t_scope + std::time::Duration::from_secs(chunk_gap_secs);
            let t_end = t_chunk + std::time::Duration::from_secs(end_gap_secs);

            let v = trace_timing_json(t0, t_config, Some(t_scope), Some(t_chunk), Some(t_end));
            prop_assert!(v.get("config_load_us").is_some());
            prop_assert!(v.get("scope_eval_us").is_some());
            prop_assert!(v.get("prep_us").is_some());
            prop_assert!(v.get("mcp_us").is_some());
        }
    }

    // #920: `memory_url` must use the disk-persisted merge cache when its key
    // matches, instead of redoing the live merge. Proven behaviorally (not by
    // spying on `merge()`): the bundle's `bundle.yaml` declares no memory/host
    // at all, so a live merge would yield `all_memory = []`, `all_host = {}`,
    // and `memory_url` would resolve to `Ok(None)` (no `still` entry to
    // select). The persisted cache is seeded — under the exact key
    // `memory_url` will independently recompute — with a `still` memory/host
    // pair a live merge could never produce from this bundle. If `memory_url`
    // returns that persisted URL, the disk-cache path was taken.
    #[test]
    fn memory_url_uses_persisted_cache_when_key_matches() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        // No `features`/`host` block — a live merge of this bundle contributes
        // no memory/host entries at all.
        std::fs::write(bundle_dir.join("bundle.yaml"), "permissions: {}\n").expect("test");

        let cache_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };

        let active = crate::scope::ActiveScopes {
            scopes: vec![],
            tags: std::collections::BTreeSet::from([
                "mytag".to_string(),
                "network-home".to_string(),
            ]),
            extra_tags: std::collections::BTreeSet::new(),
        };

        // Seed the persisted cache under the exact key `memory_url` will
        // independently recompute for this config/bundle set — derived via
        // `crate::cli::build_bundle_refs`, the same ref-builder `memory_url`
        // itself calls, so this test can't drift from production behavior.
        let firing = crate::cli::firing_bundles(&config.bundle, &active, None);
        let bundle_refs = crate::cli::build_bundle_refs(config_root.path(), &active, &firing);
        let key = crate::merge::merge_signature(&config.capabilities, &config.native, &bundle_refs)
            .expect("test");
        let persisted_memory = vec![crate::config::Memory {
            server_host: "still".into(),
            port: 7878,
            listen_host: "127.0.0.1".into(),
            when: vec!["network-home".into()],
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: std::collections::BTreeMap::new(),
            retention: None,
            auto_prune: false,
            consolidation: None,
            mcp_permissions: None,
            wakeup_max_tokens: None,
        }];
        let mut persisted_host = std::collections::BTreeMap::new();
        persisted_host.insert(
            "still".to_string(),
            crate::config::HostEntry {
                addr: "still.local".into(),
            },
        );
        crate::materialize::merge_cache::write(
            cache_dir.path(),
            &key,
            &persisted_memory,
            &persisted_host,
        )
        .expect("test");

        let url = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(
            url,
            MemoryEndpoint::Active {
                url: "http://still.local:7878/mcp".into(),
                wakeup_max_tokens: None,
            },
            "memory_url must read the persisted merge cache instead of falling \
             back to a live merge of a bundle that declares no memory/host"
        );
    }

    // #920: a stale/mismatched persisted cache must never be trusted — a false
    // hit here would silently resolve to the wrong ICM memory endpoint. This
    // seeds a persisted cache under a key that does NOT match the live
    // signature (bundle.yaml content differs from what was hashed) and
    // confirms `memory_url` falls back to a correct live merge instead.
    #[test]
    fn memory_url_ignores_persisted_cache_on_key_mismatch() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  memory:\n",
                "    - server_host: still\n",
                "      port: 7878\n",
                "      when: [network-home]\n",
                "host:\n",
                "  still:\n",
                "    addr: still.local\n",
            ),
        )
        .expect("test");

        let cache_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![],
            tags: std::collections::BTreeSet::from([
                "mytag".to_string(),
                "network-home".to_string(),
            ]),
            extra_tags: std::collections::BTreeSet::new(),
        };

        // Seed a cache entry under a deliberately wrong key, pointing at a
        // host a correct live merge would never resolve to.
        let mut bogus_host = std::collections::BTreeMap::new();
        bogus_host.insert(
            "still".to_string(),
            crate::config::HostEntry {
                addr: "bogus.invalid".into(),
            },
        );
        crate::materialize::merge_cache::write(cache_dir.path(), "wrong-key", &[], &bogus_host)
            .expect("test");

        let url = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(
            url,
            MemoryEndpoint::Active {
                url: "http://still.local:7878/mcp".into(),
                wakeup_max_tokens: None,
            },
            "a key mismatch must fall back to a correct live merge, never the stale cache"
        );
    }

    // #1133: the detached memory children were spawned with
    // `stderr(Stdio::null())`, so nothing they reported could reach anyone —
    // including the `tracing` events meant to compensate, whose sink is that
    // same discarded stderr.
    #[test]
    fn redirect_stderr_to_bounded_log_captures_child_stderr() {
        let dir = tempfile::tempdir().expect("test");
        let log = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2");
        redirect_stderr_to_bounded_log(
            &mut cmd,
            &log,
            crate::mcp::proxy::LogDirMode::OwnerOnly,
            "test",
        );

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log)
            .expect("a detached child's stderr must reach a file, not /dev/null");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }

    // Pins the shared log name: the three detached children, the docs, and any
    // operator told where to look must all agree on one path.
    #[test]
    fn detached_child_log_path_is_named_under_the_state_dir() {
        let path = detached_child_log_path().expect("test");
        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("detached-hook.log")
        );
        assert!(path.starts_with(crate::paths::state_dir().expect("test")));
    }

    /// End-to-end coverage for `redirect_stderr_to_detached_log` itself, not
    /// just the `redirect_stderr_to_bounded_log` helper it delegates to
    /// (`redirect_stderr_to_bounded_log_captures_child_stderr` above already
    /// covers that) — this calls the exact same function signature real
    /// callers do, with an injected path resolver instead of the real
    /// `detached_child_log_path`, so it's the one test that would catch this
    /// function's own body being replaced wholesale.
    #[test]
    fn redirect_stderr_to_detached_log_writes_to_the_resolved_path() {
        let dir = tempfile::tempdir().expect("test");
        let log_path = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2");
        redirect_stderr_to_detached_log(&mut cmd, || Ok(log_path.clone()));

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log_path)
            .expect("a detached child's stderr must reach the resolved log path");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }

    /// Fixture for the two #1132 failure modes: a firing bundle whose
    /// `bundle.yaml` is a *directory*, so every read of it fails with something
    /// other than NotFound — the portable stand-in for the unreadable or
    /// permission-denied bundle file whose error used to be swallowed.
    fn unreadable_bundle_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        crate::config::Config,
        crate::scope::ActiveScopes,
    ) {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(bundle_dir.join("bundle.yaml")).expect("test");

        let cache_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![],
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            extra_tags: std::collections::BTreeSet::new(),
        };
        (config_root, cache_dir, config, active)
    }

    // #1132: both of `resolve_bundle_memory_host`'s failure modes used to be
    // discarded — the live merge via `unwrap_or_default()` (so a malformed or
    // unreadable `bundle.yaml` collapsed to "no bundle memory" and the user was
    // told `no memory backend active for this scope`, pointing them at their
    // scope config, the one place the problem wasn't), and the cache-key
    // computation via `.ok()` (so the #920 optimization silently never engaged,
    // visible only as unexplained hook latency).
    //
    // Both are asserted in one test on purpose: `tracing` caches a callsite's
    // interest globally on first hit, so a sibling test reaching the same
    // `warn!` outside any subscriber would make a separate log-capture test
    // order-dependent.
    #[test]
    fn memory_url_reports_both_bundle_merge_failure_modes() {
        let (config_root, _cache, config, active) = unreadable_bundle_fixture();
        let log_dir = tempfile::tempdir().expect("test");
        let log = log_dir.path().join("events.jsonl");
        // ERROR-only, matching main.rs's `EnvFilter::from_default_env()` with
        // `RUST_LOG` unset (#1139): a `warn!` here would prove nothing about
        // what an operator actually sees by default.
        let result = crate::session_log::tracing_layer::capture_file_logs_at(
            &log,
            tracing_subscriber::filter::LevelFilter::ERROR,
            || memory_url(&config, config_root.path(), &active),
        );

        let err = result
            .expect_err("a bundle merge failure must reach the caller, not be defaulted away");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bundle 'b'"),
            "the error must name the bundle whose merge failed: {msg}"
        );

        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            body.contains("merge signature"),
            "a signature failure must be logged at a level the default EnvFilter \
             passes, before falling back to a live merge: {body}"
        );
    }

    // #1139: `memory_url(...)?.into_url()` at the call site put `?` directly
    // after `memory_url`, propagating *its* Err (a bundle-merge failure, #1132)
    // out of the enclosing function before `.into_url()` ever ran — bypassing
    // the fail-soft handling and aborting the whole hook event (session
    // logging included) on a merge failure. `resolve_memory_client` returns
    // `Option`, not `Result`, so it cannot repeat that mistake by construction.
    #[test]
    fn resolve_memory_client_does_not_propagate_a_bundle_merge_failure() {
        let (config_root, _cache, config, active) = unreadable_bundle_fixture();
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client =
            resolve_memory_client(&config, config_root.path(), &active, "test-event", &CACHE);

        assert!(
            client.is_none(),
            "a bundle merge failure must degrade to no client, never propagate"
        );
    }

    // #1125: the recall-keyword/context-chunk bundle list is a second selection
    // that also ignored `disable_bundles`, so a bundle a project opts out of
    // still became an `llmenv-bundle:<name>` recall query and appeared in the
    // context chunk stored in the memory backend.
    #[test]
    fn recall_bundle_names_excludes_disabled_bundles() {
        let scope = |id: &str, enable: &[&str], disable: &[&str]| crate::scope::ActiveScope {
            id: id.to_string(),
            kind: "project",
            tags: vec![],
            project_root: None,
            enable_bundles: enable.iter().map(|s| (*s).to_string()).collect(),
            disable_bundles: disable.iter().map(|s| (*s).to_string()).collect(),
            name: None,
            description: None,
            unknown_fields: vec![],
        };

        let active = crate::scope::ActiveScopes {
            scopes: vec![
                scope("user", &["yaks", "rust-dev"], &[]),
                scope("project", &["web-dev"], &["yaks"]),
            ],
            ..Default::default()
        };

        assert_eq!(
            recall_bundle_names(&active),
            vec!["rust-dev".to_string(), "web-dev".to_string()],
            "a disabled bundle must not reach recall queries or the stored chunk, \
             even when another scope enables it"
        );
    }

    #[test]
    fn recall_bundle_names_dedupes_and_sorts() {
        let scope = |id: &str, enable: &[&str]| crate::scope::ActiveScope {
            id: id.to_string(),
            kind: "user",
            tags: vec![],
            project_root: None,
            enable_bundles: enable.iter().map(|s| (*s).to_string()).collect(),
            disable_bundles: vec![],
            name: None,
            description: None,
            unknown_fields: vec![],
        };

        let active = crate::scope::ActiveScopes {
            scopes: vec![scope("a", &["zed", "alpha"]), scope("b", &["alpha"])],
            ..Default::default()
        };

        assert_eq!(
            recall_bundle_names(&active),
            vec!["alpha".to_string(), "zed".to_string()]
        );
    }

    /// A project scope that disables every bundle in `disable`.
    fn disabling_project_scope(disable: &[&str]) -> crate::scope::ActiveScope {
        crate::scope::ActiveScope {
            id: "project".into(),
            kind: "project",
            tags: vec![],
            project_root: None,
            enable_bundles: vec![],
            disable_bundles: disable.iter().map(|s| (*s).to_string()).collect(),
            name: None,
            description: None,
            unknown_fields: vec![],
        }
    }

    // #1131: with no bundles and no `features.memory` at all, nothing could have
    // supplied a backend — distinct from "a bundle would have, but is disabled".
    #[test]
    fn memory_url_reports_no_bundles_fired_when_nothing_is_configured() {
        let config_root = tempfile::tempdir().expect("test");
        let config = crate::config::Config::default();
        let active = crate::scope::ActiveScopes::default();

        assert_eq!(
            memory_url(&config, config_root.path(), &active).expect("test"),
            MemoryEndpoint::NoBundlesFired
        );
    }

    // #1216: a top-level `features.memory` entry's configured
    // `wakeup_max_tokens` must survive resolution and be readable off the
    // active endpoint, so hook_run's dispatch pipeline can pass it to
    // icm_wake_up.
    #[test]
    fn memory_url_surfaces_configured_wakeup_max_tokens() {
        let config_root = tempfile::tempdir().expect("test");
        let mut host = std::collections::BTreeMap::new();
        host.insert(
            "still".to_string(),
            crate::config::HostEntry {
                addr: "still.local".into(),
            },
        );
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                memory: vec![crate::config::Memory {
                    server_host: "still".into(),
                    port: 7878,
                    listen_host: "127.0.0.1".into(),
                    when: vec!["network-home".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: std::collections::BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                    mcp_permissions: None,
                    wakeup_max_tokens: Some(750),
                }],
                ..Default::default()
            }),
            host,
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            tags: std::collections::BTreeSet::from(["network-home".to_string()]),
            ..Default::default()
        };

        let endpoint = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(endpoint.wakeup_max_tokens(), Some(750));
    }

    // #1140: a top-level `features.memory` entry exists but its `when` isn't
    // in the active scope. Before this fix, `resolve_mcps`'s `0 => {}` arm
    // dropped it silently and this collapsed into `NoBundlesFired` — telling
    // the user "config.yaml declares no features.memory" when it plainly
    // does, just gated on a tag that isn't active right now.
    #[test]
    fn memory_url_reports_tag_inactive_when_declared_entry_is_ungated() {
        let config_root = tempfile::tempdir().expect("test");
        let mut host = std::collections::BTreeMap::new();
        host.insert(
            "still".to_string(),
            crate::config::HostEntry {
                addr: "still.local".into(),
            },
        );
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                memory: vec![crate::config::Memory {
                    server_host: "still".into(),
                    port: 7878,
                    listen_host: "127.0.0.1".into(),
                    when: vec!["network-home".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: std::collections::BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                    mcp_permissions: None,
                    wakeup_max_tokens: None,
                }],
                ..Default::default()
            }),
            host,
            ..Default::default()
        };
        // Active scope carries no tags at all — `network-home` isn't among them.
        let active = crate::scope::ActiveScopes::default();

        let resolved = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(
            resolved,
            MemoryEndpoint::TagInactive {
                server_hosts: vec!["still".to_string()]
            },
            "a declared-but-tag-inactive entry must not collapse into \
             NoBundlesFired, got {resolved:?}"
        );
        let msg = resolved
            .into_url()
            .expect_err("test")
            .to_string()
            .to_lowercase();
        assert!(
            msg.contains("still") && msg.contains("when"),
            "the message must name the server_host and the tag gating: {msg}"
        );
    }

    // #1131: bundles fired, they just declare no memory — the benign case, and
    // the only one the old `Ok(None)` reading was ever right about.
    #[test]
    fn memory_url_reports_not_declared_when_firing_bundle_has_no_memory() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        std::fs::write(bundle_dir.join("bundle.yaml"), "permissions: {}\n").expect("test");

        let cache_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            ..Default::default()
        };

        assert_eq!(
            memory_url(&config, config_root.path(), &active).expect("test"),
            MemoryEndpoint::NotDeclared {
                skipped_bundles: vec![]
            }
        );
    }

    // #1133: a firing bundle with no content directory is dropped by
    // `build_bundle_refs` with a `warn!` the default `EnvFilter` discards — a
    // third unlit road to "no memory backend". Named here instead, where it has
    // a consequence the user can see.
    #[test]
    fn memory_url_names_firing_bundles_skipped_for_missing_content_dir() {
        let config_root = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "ghost".into(),
                when: vec!["mytag".into()],
            }],
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            ..Default::default()
        };

        assert_eq!(
            memory_url(&config, config_root.path(), &active).expect("test"),
            MemoryEndpoint::NotDeclared {
                skipped_bundles: vec!["ghost".to_string()]
            }
        );
    }

    // #1142: `build_bundle_refs` drops a firing bundle for two distinct
    // reasons — no content directory, or an unsafe/traversal name — and the
    // old message asserted the first reason unconditionally. The message must
    // no longer claim a specific cause it can't actually attribute.
    #[test]
    fn memory_url_reports_notdeclared_message_covers_rejected_names_too() {
        let config_root = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "../evil".into(),
                when: vec!["mytag".into()],
            }],
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            ..Default::default()
        };

        let resolved = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(
            resolved,
            MemoryEndpoint::NotDeclared {
                skipped_bundles: vec!["../evil".to_string()]
            }
        );
        let msg = resolved
            .into_url()
            .expect_err("test")
            .to_string()
            .to_lowercase();
        assert!(
            msg.contains("rejected") && msg.contains("content directory"),
            "the message must not claim 'missing content directory' as the sole \
             cause when a rejected name is just as plausible: {msg}"
        );
    }

    // #1131: the disabled-bundle case must name the bundle that would have
    // supplied the backend, so the resulting message can point at
    // `disable_bundles` instead of the user's scope/tag config.
    #[test]
    fn memory_url_reports_disable_bundles_as_the_cause() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  memory:\n",
                "    - server_host: still\n",
                "      port: 7878\n",
                "      when: [network-home]\n",
                "host:\n",
                "  still:\n",
                "    addr: still.local\n",
            ),
        )
        .expect("test");

        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![disabling_project_scope(&["b"])],
            tags: std::collections::BTreeSet::from([
                "mytag".to_string(),
                "network-home".to_string(),
            ]),
            ..Default::default()
        };

        let resolved = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(
            resolved,
            MemoryEndpoint::SuppressedByDisableBundles(vec!["b".to_string()]),
            "the sole source of features.memory being disabled must be reported \
             as such, not collapsed into `no memory backend active`"
        );
        let msg = resolved
            .into_url()
            .expect_err("test")
            .to_string()
            .to_lowercase();
        assert!(
            msg.contains("disable_bundles") && msg.contains('b'),
            "the message must name disable_bundles and the bundle: {msg}"
        );
    }

    // #1140/#1141 (pre-pr-review of #1234): a disabled bundle whose only
    // `features.memory` entry is itself tag-inactive supplies nothing even if
    // re-enabled, so it must not be named as the cause. Before this fix,
    // `suppressed_memory_bundles` checked mere presence rather than
    // tag-activity — the exact bug the sibling `declares_active_memory` fix
    // (doctor.rs) exists to prevent, just reached through the shared helper.
    #[test]
    fn memory_url_ignores_disabled_bundle_whose_memory_entry_is_tag_inactive() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  memory:\n",
                "    - server_host: still\n",
                "      port: 7878\n",
                "      when: [othertag]\n",
                "host:\n",
                "  still:\n",
                "    addr: still.local\n",
            ),
        )
        .expect("test");

        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![disabling_project_scope(&["b"])],
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            ..Default::default()
        };

        assert_eq!(
            memory_url(&config, config_root.path(), &active).expect("test"),
            MemoryEndpoint::NoBundlesFired,
            "a disabled bundle whose sole memory entry is gated on an inactive \
             tag would supply nothing even if re-enabled, so it must not be \
             reported as the (fixable) cause"
        );
    }

    // #1131: a top-level `features.memory` whose `server_host` lives only in a
    // disabled bundle's `host:` table hard-errors with a host the user can see
    // in their own config and nothing pointing at `disable_bundles`.
    #[test]
    fn memory_url_unknown_server_host_error_mentions_disable_bundles() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "host:\n  still:\n    addr: still.local\n",
        )
        .expect("test");

        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            features: Some(crate::config::Features {
                memory: vec![crate::config::Memory {
                    server_host: "still".into(),
                    port: 7878,
                    listen_host: "127.0.0.1".into(),
                    when: vec!["mytag".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: std::collections::BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                    mcp_permissions: None,
                    wakeup_max_tokens: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![disabling_project_scope(&["b"])],
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            ..Default::default()
        };

        let msg = format!(
            "{:#}",
            memory_url(&config, config_root.path(), &active).expect_err("test")
        );
        assert!(
            msg.contains("disable_bundles") && msg.contains("still"),
            "an unknown server_host supplied by a disabled bundle must say so: {msg}"
        );
    }

    // #1125: `memory_url` used to compute its firing-bundle set with an inline
    // filter that checked tag-match and `enable_bundles` but never
    // `disable_bundles`, so a bundle a project scope explicitly turns off still
    // contributed its `features.memory`/`host` entries to ICM endpoint
    // resolution — diverging from the materialized manifest, which excludes it.
    #[test]
    fn memory_url_ignores_bundle_disabled_by_project_scope() {
        let config_root = tempfile::tempdir().expect("test");
        let bundle_dir = config_root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).expect("test");
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  memory:\n",
                "    - server_host: still\n",
                "      port: 7878\n",
                "      when: [network-home]\n",
                "host:\n",
                "  still:\n",
                "    addr: still.local\n",
            ),
        )
        .expect("test");

        let cache_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };

        // The aggregated tag set fires the bundle; the project scope disables it.
        let active = crate::scope::ActiveScopes {
            scopes: vec![crate::scope::ActiveScope {
                id: "project".into(),
                kind: "project",
                tags: vec![],
                project_root: None,
                enable_bundles: vec![],
                disable_bundles: vec!["b".into()],
                name: None,
                description: None,
                unknown_fields: vec![],
            }],
            tags: std::collections::BTreeSet::from([
                "mytag".to_string(),
                "network-home".to_string(),
            ]),
            ..Default::default()
        };

        let url = memory_url(&config, config_root.path(), &active).expect("test");
        assert!(
            !matches!(url, MemoryEndpoint::Active { .. }),
            "a bundle disabled via `disable_bundles` must not contribute its \
             memory/host entries to memory_url resolution, got {url:?}"
        );
    }

    fn index_repository_clobber_payload() -> serde_json::Value {
        serde_json::json!({
            "tool_name": crate::hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL,
            "tool_input": { "repo_path": "/repo", "name": "some-other-project" },
        })
    }

    // #1331: `repeat_detect` matches every tool, so it can produce advisory
    // text for this exact call. If the two were joined, the advisory would
    // land ahead of `__DENY__:` and `run()`'s prefix check would miss it —
    // the deny would silently become an allow.
    #[test]
    fn index_repository_clobber_deny_is_not_diluted_by_repeat_detect() {
        let state_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: true,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let payload = index_repository_clobber_payload();
        // Twice, so repeat_detect has a prior call to match against.
        for _ in 0..2 {
            let text = resolve_pre_tool_decision(
                &payload,
                Some("clobber"),
                &config,
                false,
                Ok(state_dir.path().to_path_buf()),
            );
            let text = text.expect("the guard must decide this call");
            assert!(
                text.starts_with("__DENY__:"),
                "the deny must lead the string, got {text:?}"
            );
        }
    }

    // The guard reads only the call's arguments, so losing the state dir must
    // not be able to turn a clobbering call into an allowed one.
    #[test]
    fn index_repository_clobber_deny_survives_a_state_dir_failure() {
        let text = resolve_pre_tool_decision(
            &index_repository_clobber_payload(),
            Some("clobber"),
            &crate::config::Config::default(),
            true,
            Err(anyhow::anyhow!("no state dir")),
        );
        assert!(
            text.is_some_and(|t| t.starts_with("__DENY__:")),
            "a state-dir failure must not disable the clobber guard"
        );
    }

    #[test]
    fn index_repository_without_a_name_still_reaches_the_normal_pipeline() {
        let state_dir = tempfile::tempdir().expect("test");
        let text = resolve_pre_tool_decision(
            &serde_json::json!({
                "tool_name": crate::hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL,
                "tool_input": { "repo_path": "/repo" },
            }),
            Some("plain"),
            &crate::config::Config::default(),
            false,
            Ok(state_dir.path().to_path_buf()),
        );
        assert!(
            !text.is_some_and(|t| t.starts_with("__DENY__:")),
            "llmenv's own auto-index shape must not be denied"
        );
    }

    /// #1006: `resolve_pre_tool_text` must not let an enabled `read_once`
    /// permanently mask `repeat_detect` for tool calls `read_once` doesn't
    /// cover — regression test for the exact bug fp-check confirmed during
    /// pre-pr-review (read_once's handler returning `""` for non-`Read`
    /// tools used to still count as "a decision was made").
    #[test]
    fn repeat_detect_fires_even_when_read_once_is_also_enabled() {
        let session_id = "mask";
        let state_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                read_once: Some(crate::config::ReadOnce {
                    enabled: true,
                    mode: crate::config::ReadOnceMode::Warn,
                    ttl_seconds: 1200,
                }),
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: true,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "echo hi" },
        });
        let text =
            resolve_pre_tool_text(&payload, Some(session_id), &config, false, state_dir.path());
        assert!(
            text.is_some_and(|t| !t.is_empty()),
            "repeat_detect must still fire for a non-Read tool when read_once is also enabled"
        );
    }

    // #976: cd_guard is on by default and composes with repeat_detect, same
    // as read_once does — both texts must survive, joined.
    #[test]
    fn cd_guard_fires_by_default_for_a_cd_command() {
        let state_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config::default();
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cd /tmp && ls" },
        });
        let text = resolve_pre_tool_text(&payload, Some("s1"), &config, false, state_dir.path());
        assert!(
            text.is_some_and(|t| t.contains("cd")),
            "cd_guard must be on by default"
        );
    }

    #[test]
    fn cd_guard_disabled_via_config_stays_silent() {
        let state_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                cd_guard: Some(crate::config::CdGuard { enabled: false }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cd /tmp && ls" },
        });
        let text = resolve_pre_tool_text(&payload, Some("s1"), &config, false, state_dir.path());
        assert!(
            text.is_none(),
            "cd_guard disabled and nothing else configured must produce no advisory"
        );
    }

    #[test]
    fn cd_guard_composes_with_repeat_detect_text() {
        let state_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: true,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cd /tmp && ls" },
        });
        let text = resolve_pre_tool_text(&payload, Some("s2"), &config, false, state_dir.path())
            .expect("test");
        assert!(text.contains("cd"), "cd_guard text must be present: {text}");
        assert!(
            text.contains("identical input"),
            "repeat_detect text must also be present: {text}"
        );
    }

    #[test]
    fn read_once_still_wins_for_read_tool_over_repeat_detect() {
        // Both features want to say something about the 2nd identical Read
        // (read_once: "already read"; repeat_detect, threshold 1: fires on
        // every call). read_once is checked first, so its message must win.
        let session_id = "rw";
        let dir = tempfile::tempdir().expect("test");
        let file_path = dir.path().join("f.txt");
        std::fs::write(&file_path, "hello").expect("test");
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                read_once: Some(crate::config::ReadOnce {
                    enabled: true,
                    mode: crate::config::ReadOnceMode::Warn,
                    ttl_seconds: 1200,
                }),
                repeat_detect: Some(crate::config::RepeatDetect {
                    enabled: true,
                    threshold: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let payload = serde_json::json!({
            "tool_name": "Read",
            "tool_input": { "file_path": file_path.to_str().unwrap() },
        });
        resolve_pre_tool_text(&payload, Some(session_id), &config, false, dir.path());
        let second = resolve_pre_tool_text(&payload, Some(session_id), &config, false, dir.path());
        assert!(
            second.is_some_and(|t| t.contains("already read")),
            "read_once's advisory must win over repeat_detect for a Read tool call"
        );
    }

    /// #1109: the task-tracker branch must honor the injected `state_dir`,
    /// matching the isolation #1089 gave `read_once`/`repeat_detect`. Without
    /// it this test writes a task into the developer's real `llmenv task`
    /// tracker instead of the tempdir.
    #[test]
    fn task_tracker_redirect_writes_only_to_injected_state_dir() {
        let dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config::default();
        let payload = serde_json::json!({
            "tool_name": "TaskCreate",
            "tool_input": { "subject": "isolated task" },
        });
        let text = resolve_pre_tool_text(&payload, Some("iso"), &config, true, dir.path())
            .expect("the task-tool redirect always decides");
        let tasks = crate::task::list_tasks(dir.path());
        assert_eq!(tasks.len(), 1, "task must land in the injected state_dir");
        assert_eq!(tasks[0].title, "isolated task");
        // Pins the success arm specifically: every failure arm also denies, so
        // the `__DENY__:` prefix alone wouldn't tell them apart.
        assert!(
            text.starts_with("__DENY__:") && text.contains(&tasks[0].slug),
            "deny must name the created task: {text}"
        );
    }

    #[test]
    fn index_repository_command_sets_env_and_args() {
        let cm = crate::config::CodebaseMemory {
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        let cmd = build_index_repository_command(
            std::path::Path::new("/repos/proj"),
            &cm,
            std::path::Path::new("/state"),
        );
        assert_eq!(cmd.get_program().to_string_lossy(), "codebase-memory-mcp");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "cli");
        assert_eq!(args[1], "index_repository");
        assert!(args[2].contains("/repos/proj"));
        let envs: std::collections::BTreeMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().to_string(),
                        v.to_string_lossy().to_string(),
                    )
                })
            })
            .collect();
        assert_eq!(
            envs.get("CBM_ALLOWED_ROOT").map(String::as_str),
            Some("/repos/proj")
        );
        assert_eq!(
            envs.get("CBM_CACHE_DIR").map(String::as_str),
            Some("/state/codebase-memory")
        );
    }

    #[test]
    fn index_repository_command_index_path_override_wins() {
        let cm = crate::config::CodebaseMemory {
            when: vec!["proj".to_string()],
            index_path: Some("/custom/path".to_string()),
            mcp_permissions: None,
        };
        let cmd = build_index_repository_command(
            std::path::Path::new("/repos/proj"),
            &cm,
            std::path::Path::new("/state"),
        );
        let envs: std::collections::BTreeMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().to_string(),
                        v.to_string_lossy().to_string(),
                    )
                })
            })
            .collect();
        assert_eq!(
            envs.get("CBM_CACHE_DIR").map(String::as_str),
            Some("/custom/path")
        );
    }

    #[test]
    fn codebase_memory_cache_dir_defaults_under_state_dir() {
        let cm = crate::config::CodebaseMemory {
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        assert_eq!(
            codebase_memory_cache_dir(&cm, std::path::Path::new("/state")),
            std::path::PathBuf::from("/state/codebase-memory")
        );
    }

    #[test]
    fn codebase_memory_cache_dir_honors_index_path_override() {
        let cm = crate::config::CodebaseMemory {
            when: vec!["proj".to_string()],
            index_path: Some("/custom/path".to_string()),
            mcp_permissions: None,
        };
        assert_eq!(
            codebase_memory_cache_dir(&cm, std::path::Path::new("/state")),
            std::path::PathBuf::from("/custom/path")
        );
    }

    /// #1091: the indexer's stderr must land in a size-bounded, owner-only
    /// log file rather than `/dev/null`, so a failing multi-minute index run
    /// is diagnosable. Exercises the real `trigger_codebase_memory_index`
    /// entry point — the `codebase-memory-mcp` binary need not actually be
    /// installed: whether `spawn()` succeeds or fails soft, the log file
    /// must already exist with the right properties, since it's opened
    /// before the spawn is attempted.
    #[cfg(unix)]
    #[test]
    fn trigger_codebase_memory_index_creates_owner_only_bounded_log() {
        use std::os::unix::fs::PermissionsExt;
        let state_dir = tempfile::tempdir().unwrap();
        let cm = crate::config::CodebaseMemory {
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        trigger_codebase_memory_index(std::path::Path::new("/repos/proj"), &cm, state_dir.path());

        let log_path = state_dir.path().join("codebase-memory").join("index.log");
        let meta = std::fs::metadata(&log_path)
            .unwrap_or_else(|e| panic!("expected {} to exist: {e}", log_path.display()));
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    // #1196: a user-configured `index_path` can be shared with a
    // codebase-memory-mcp process running under a different uid (separate
    // service account, container with different uid mapping). Forcing it to
    // 0700 — appropriate for llmenv's own state tree — breaks that sharing
    // with an EACCES surfaced only via `tracing::debug!`. Only the *default*
    // (state_dir-rooted) cache dir gets hardened; an explicit override keeps
    // whatever permissions its owner already gave it.
    #[cfg(unix)]
    #[test]
    fn trigger_codebase_memory_index_leaves_user_index_path_permissions_alone() {
        use std::os::unix::fs::PermissionsExt;
        let state_dir = tempfile::tempdir().unwrap();
        let index_dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(index_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let cm = crate::config::CodebaseMemory {
            when: vec!["proj".to_string()],
            index_path: Some(index_dir.path().to_str().unwrap().to_string()),
            mcp_permissions: None,
        };

        trigger_codebase_memory_index(std::path::Path::new("/repos/proj"), &cm, state_dir.path());

        let mode = std::fs::metadata(index_dir.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "a user-configured index_path must keep its prior permissions, got {mode:o}"
        );
    }

    // PRELOADED_CONFIG is a process-global cache; these two tests populate and
    // clear it via `reset_preloaded_config_for_test`, so they must not run
    // concurrently with each other (cargo test runs test fns in parallel by
    // default) or they'd race on the shared state. This lock — test-only,
    // unrelated to PRELOADED_CONFIG's own storage — serializes just these two.
    static PRELOADED_CONFIG_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn load_cached_config_falls_back_to_disk_when_nothing_preloaded() {
        let _guard = PRELOADED_CONFIG_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_preloaded_config_for_test();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            serde_yaml::to_string(&crate::config::Config::default()).unwrap(),
        )
        .unwrap();
        let loaded = load_cached_config(&path).unwrap();
        assert_eq!(loaded, crate::config::Config::default());
    }

    #[test]
    fn load_cached_config_returns_preloaded_config_when_set() {
        let _guard = PRELOADED_CONFIG_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_preloaded_config_for_test();

        let mut preloaded = crate::config::Config::default();
        preloaded.disabled_engines.push("preload-marker".into());
        set_preloaded_config(preloaded.clone());

        // The path on disk holds a *different* config, so a correct result
        // proves load_cached_config took the preload, not a fresh disk read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            serde_yaml::to_string(&crate::config::Config::default()).unwrap(),
        )
        .unwrap();

        let loaded = load_cached_config(&path).unwrap();
        assert_eq!(loaded, preloaded);

        reset_preloaded_config_for_test();
    }

    proptest::proptest! {
        // #365: the repo_path JSON arg must always be valid JSON that
        // round-trips to the exact project_root string, for any path shape
        // (quotes, backslashes, unicode, whitespace) — directly validates
        // that this is safe against injection into the subprocess argv
        // (pre-pr-review finding).
        #[test]
        fn index_repository_command_json_arg_always_valid_and_roundtrips(
            path_str in "[\\PC]{0,60}"
        ) {
            let cm = crate::config::CodebaseMemory {
                when: vec!["proj".to_string()],
                index_path: None,
                mcp_permissions: None,
            };
            let project_root = std::path::PathBuf::from(&path_str);
            let cmd = build_index_repository_command(
                &project_root,
                &cm,
                std::path::Path::new("/state"),
            );
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            let parsed: serde_json::Value = serde_json::from_str(&args[2])
                .expect("repo_path arg must always be valid JSON");
            let expected = project_root.display().to_string();
            proptest::prop_assert_eq!(parsed["repo_path"].as_str(), Some(expected.as_str()));
        }
    }

    #[test]
    fn append_read_once_result_appends_advisory_to_empty_out() {
        let mut out = String::new();
        append_read_once_result(&mut out, "advisory text");
        assert_eq!(out, "advisory text");
    }

    #[test]
    fn append_read_once_result_appends_deny_to_empty_out() {
        let mut out = String::new();
        append_read_once_result(&mut out, "__DENY__:already read");
        assert_eq!(out, "__DENY__:already read");
    }

    #[test]
    fn append_read_once_result_appends_advisory_to_nonempty_out() {
        let mut out = String::from("existing content");
        append_read_once_result(&mut out, "advisory text");
        assert_eq!(out, "existing content\nadvisory text");
    }

    // #868: `out` being non-empty before a deny append means dispatch()
    // started producing PreToolUse actions, which run()'s positional
    // __DENY__: prefix check can't safely combine with — the deny must
    // replace `out` entirely (in release builds too) rather than silently
    // downgrading to an allow.
    #[test]
    fn append_read_once_result_deny_replaces_nonempty_out() {
        let mut out = String::from("existing content");
        append_read_once_result(&mut out, "__DENY__:already read");
        assert_eq!(out, "__DENY__:already read");
    }

    #[test]
    fn parses_neutral_event_names() {
        assert_eq!(
            "session_start".parse::<HookEvent>().unwrap(),
            HookEvent::SessionStart
        );
        assert_eq!(
            "turn_start".parse::<HookEvent>().unwrap(),
            HookEvent::TurnStart
        );
        assert_eq!(
            "session_end".parse::<HookEvent>().unwrap(),
            HookEvent::SessionEnd
        );
        assert_eq!(
            "post_session".parse::<HookEvent>().unwrap(),
            HookEvent::PostSession
        );
    }

    #[test]
    fn rejects_unknown_event() {
        assert!("nope".parse::<HookEvent>().is_err());
    }

    #[test]
    fn parses_verbose_event_names() {
        assert_eq!(
            "user_prompt_submit".parse::<HookEvent>().unwrap(),
            HookEvent::UserPromptSubmit
        );
        assert_eq!(
            "pre_tool_use".parse::<HookEvent>().unwrap(),
            HookEvent::PreToolUse
        );
        assert_eq!(
            "post_tool_use".parse::<HookEvent>().unwrap(),
            HookEvent::PostToolUse
        );
        assert_eq!(
            "notification".parse::<HookEvent>().unwrap(),
            HookEvent::Notification
        );
        assert_eq!("stop".parse::<HookEvent>().unwrap(), HookEvent::Stop);
        assert_eq!(
            "subagent_stop".parse::<HookEvent>().unwrap(),
            HookEvent::SubagentStop
        );
        assert_eq!(
            "pre_compact".parse::<HookEvent>().unwrap(),
            HookEvent::PreCompact
        );
    }

    #[test]
    fn verbose_event_display_round_trips_through_from_str() {
        for ev in [
            HookEvent::SessionStart,
            HookEvent::TurnStart,
            HookEvent::SessionEnd,
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::Notification,
            HookEvent::Stop,
            HookEvent::SubagentStop,
            HookEvent::PreCompact,
        ] {
            assert_eq!(ev.to_string().parse::<HookEvent>().unwrap(), ev);
        }
    }

    #[test]
    fn verbose_events_map_to_log_kinds() {
        assert_eq!(
            event_to_log_kind(HookEvent::UserPromptSubmit).unwrap(),
            (EventKind::Prompt, "user")
        );
        assert_eq!(
            event_to_log_kind(HookEvent::PreToolUse).unwrap(),
            (EventKind::ToolUse, "tool")
        );
        assert_eq!(
            event_to_log_kind(HookEvent::PostToolUse).unwrap(),
            (EventKind::ToolResult, "tool")
        );
        assert_eq!(
            event_to_log_kind(HookEvent::Notification).unwrap(),
            (EventKind::Notification, "system")
        );
        assert_eq!(
            event_to_log_kind(HookEvent::Stop).unwrap(),
            (EventKind::Stop, "assistant")
        );
        assert_eq!(
            event_to_log_kind(HookEvent::SubagentStop).unwrap(),
            (EventKind::Stop, "assistant")
        );
        assert_eq!(
            event_to_log_kind(HookEvent::PreCompact).unwrap(),
            (EventKind::Notification, "system")
        );
    }

    #[test]
    fn lifecycle_and_memory_events_have_no_log_kind() {
        assert_eq!(event_to_log_kind(HookEvent::SessionStart), None);
        assert_eq!(event_to_log_kind(HookEvent::TurnStart), None);
        assert_eq!(event_to_log_kind(HookEvent::SessionEnd), None);
    }

    #[test]
    fn dispatch_emits_no_memory_actions_for_verbose_events() {
        for ev in [
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::Notification,
            HookEvent::Stop,
            HookEvent::SubagentStop,
            HookEvent::PreCompact,
        ] {
            assert_eq!(dispatch(ev, &[], &[], None), Vec::<Action>::new());
        }
    }

    #[test]
    fn verbose_content_extracts_prompt_text() {
        let payload = serde_json::json!({"prompt": "fix the bug"});
        let (tool_name, content) = event_content(HookEvent::UserPromptSubmit, &payload);
        assert_eq!(tool_name, None);
        assert_eq!(content, "fix the bug");
    }

    #[test]
    fn verbose_content_extracts_pre_tool_use_name_and_input() {
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        });
        let (tool_name, content) = event_content(HookEvent::PreToolUse, &payload);
        assert_eq!(tool_name.as_deref(), Some("Bash"));
        assert!(content.contains("\"command\":\"ls\""));
    }

    #[test]
    fn verbose_content_extracts_post_tool_use_response() {
        let payload = serde_json::json!({
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/x"},
            "tool_response": {"filePath": "/tmp/x"},
        });
        let (tool_name, content) = event_content(HookEvent::PostToolUse, &payload);
        assert_eq!(tool_name.as_deref(), Some("Write"));
        assert!(content.contains("filePath"));
    }

    #[test]
    fn verbose_content_extracts_notification_message() {
        let payload = serde_json::json!({"message": "needs your attention"});
        let (_, content) = event_content(HookEvent::Notification, &payload);
        assert_eq!(content, "needs your attention");
    }

    #[test]
    fn verbose_content_extracts_stop_last_assistant_message() {
        let payload = serde_json::json!({"last_assistant_message": "done"});
        let (_, content) = event_content(HookEvent::Stop, &payload);
        assert_eq!(content, "done");
        let (_, content) = event_content(HookEvent::SubagentStop, &payload);
        assert_eq!(content, "done");
    }

    #[test]
    fn verbose_content_extracts_pre_compact_trigger() {
        let payload = serde_json::json!({"trigger": "manual", "custom_instructions": ""});
        let (_, content) = event_content(HookEvent::PreCompact, &payload);
        assert_eq!(content, "manual");
    }

    #[test]
    fn verbose_content_is_empty_for_missing_fields() {
        let (tool_name, content) =
            event_content(HookEvent::UserPromptSubmit, &serde_json::Value::Null);
        assert_eq!(tool_name, None);
        assert_eq!(content, "");
    }

    #[test]
    fn dispatch_maps_events_to_actions() {
        assert_eq!(
            dispatch(HookEvent::SessionStart, &[], &[], None),
            vec![Action::WakeUp(None)]
        );
        assert_eq!(
            dispatch(HookEvent::TurnStart, &[], &[], None),
            vec![Action::Recall]
        );
        assert_eq!(
            dispatch(HookEvent::SessionEnd, &[], &[], None),
            vec![Action::Store]
        );
        assert_eq!(
            dispatch(HookEvent::PostSession, &[], &[], None),
            vec![],
            "PostSession defers to consolidation module, no dispatch actions"
        );
    }

    #[test]
    fn dispatch_threads_wakeup_max_tokens_into_session_start_only() {
        assert_eq!(
            dispatch(HookEvent::SessionStart, &[], &[], Some(750)),
            vec![Action::WakeUp(Some(750))]
        );
        // Not carried by any other event's actions — WakeUp only fires on SessionStart.
        assert_eq!(
            dispatch(HookEvent::TurnStart, &[], &[], Some(750)),
            vec![Action::Recall]
        );
    }

    #[test]
    fn dedup_and_count_drops_empty_and_exact_duplicate_recall_results() {
        let results = vec![
            (true, "memory A".to_string()),
            (true, String::new()),          // advisory-only, stripped to empty
            (true, "memory A".to_string()), // exact duplicate of the first
            (true, "memory B".to_string()),
        ];
        let (kept, stats) = dedup_and_count_action_results(results);
        assert_eq!(kept, vec!["memory A".to_string(), "memory B".to_string()]);
        // recall_entries only counts non-empty responses: 3 of the 4 (the
        // empty one never increments it), and recall_dropped counts the
        // empty one plus the exact-duplicate — 2 of those 3 non-empty/total.
        assert_eq!(stats.recall_entries, 3);
        assert_eq!(stats.recall_bytes, "memory A".len() * 2 + "memory B".len());
        assert_eq!(stats.recall_dropped, 2);
    }

    #[test]
    fn dedup_and_count_ignores_non_recall_actions_in_the_tally() {
        // WakeUp/Store never mix with recall actions per `dispatch`, but the
        // tally must still be scoped to `is_recall` entries if that changes.
        let results = vec![(false, "wake-up pack".to_string())];
        let (kept, stats) = dedup_and_count_action_results(results);
        assert_eq!(kept, vec!["wake-up pack".to_string()]);
        assert_eq!(stats, RecallStats::default());
    }

    #[test]
    fn emit_context_trace_never_panics_without_env_var() {
        let stats = RecallStats {
            recall_entries: 3,
            recall_bytes: 42,
            recall_dropped: 1,
        };
        emit_context_trace(&stats, &["memory A".to_string()]);
    }

    fn arb_action_result() -> impl Strategy<Value = (bool, String)> {
        (
            any::<bool>(),
            prop_oneof![Just(String::new()), "[a-z ]{1,12}"],
        )
    }

    proptest! {
        #[test]
        fn dedup_and_count_never_panics(results in prop::collection::vec(arb_action_result(), 0..8)) {
            let _ = dedup_and_count_action_results(results);
        }

        // recall_entries counts exactly the non-empty (is_recall, text) pairs —
        // independent of dedup, which only affects `kept`/`recall_dropped`.
        #[test]
        fn recall_entries_matches_non_empty_recall_input_count(
            results in prop::collection::vec(arb_action_result(), 0..8)
        ) {
            let expected = results.iter().filter(|(r, t)| *r && !t.is_empty()).count();
            let (_, stats) = dedup_and_count_action_results(results);
            prop_assert_eq!(stats.recall_entries, expected);
        }

        // kept never carries a duplicate string, and every kept string
        // actually came from the input (dedup can only drop, never invent).
        #[test]
        fn kept_has_no_duplicates_and_is_a_subset_of_input(
            results in prop::collection::vec(arb_action_result(), 0..8)
        ) {
            let texts: Vec<String> = results.iter().map(|(_, t)| t.clone()).collect();
            let (kept, _) = dedup_and_count_action_results(results);
            let mut seen = std::collections::HashSet::new();
            for text in &kept {
                prop_assert!(seen.insert(text.clone()), "kept must not repeat {text:?}");
                prop_assert!(texts.contains(text), "kept must only contain input text");
            }
        }
    }

    #[test]
    fn turn_start_expands_one_recall_tag_per_active_tag() {
        let tags = vec!["rust".to_string(), "work-vpn".to_string()];
        let queries = tag_recall_queries(&tags).expect("valid tags");
        let actions = dispatch(HookEvent::TurnStart, &queries, &[], None);
        assert_eq!(
            actions,
            vec![
                Action::Recall,
                Action::RecallTag(TagRecallQuery {
                    tag: "rust".to_string(),
                    keyword: "llmenv-tag:rust".to_string(),
                }),
                Action::RecallTag(TagRecallQuery {
                    tag: "work-vpn".to_string(),
                    keyword: "llmenv-tag:work-vpn".to_string(),
                }),
            ],
            "TurnStart must run project recall then one tag recall per active tag"
        );
    }

    #[test]
    fn turn_start_expands_one_recall_bundle_per_active_bundle() {
        let bundles = vec!["base".to_string(), "rust-defaults".to_string()];
        let queries = bundle_recall_queries(&bundles).expect("valid bundles");
        let actions = dispatch(HookEvent::TurnStart, &[], &queries, None);
        assert_eq!(
            actions,
            vec![
                Action::Recall,
                Action::RecallBundle(BundleRecallQuery {
                    bundle: "base".to_string(),
                    keyword: "llmenv-bundle:base".to_string(),
                }),
                Action::RecallBundle(BundleRecallQuery {
                    bundle: "rust-defaults".to_string(),
                    keyword: "llmenv-bundle:rust-defaults".to_string(),
                }),
            ],
            "TurnStart must emit one bundle recall per active bundle"
        );
    }

    #[test]
    fn turn_start_interleaves_tag_and_bundle_recalls() {
        let tag_qs = tag_recall_queries(&["rust".to_string()]).expect("valid");
        let bundle_qs = bundle_recall_queries(&["base".to_string()]).expect("valid");
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs, None);
        // Order: project recall, then tag recalls, then bundle recalls.
        assert_eq!(actions[0], Action::Recall);
        assert!(matches!(actions[1], Action::RecallTag(_)));
        assert!(matches!(actions[2], Action::RecallBundle(_)));
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn validate_tag_accepts_valid_tags() {
        assert!(validate_tag("base").is_ok());
        assert!(validate_tag("rust-lang").is_ok());
        assert!(validate_tag("work_project").is_ok());
        assert!(validate_tag("tag123").is_ok());
        assert!(validate_tag("my-tag_123").is_ok());
    }

    #[test]
    fn validate_tag_rejects_empty() {
        assert!(validate_tag("").is_err());
    }

    #[test]
    fn validate_tag_rejects_special_characters() {
        assert!(validate_tag("tag:space").is_err());
        assert!(validate_tag("tag space").is_err());
        assert!(validate_tag("tag/path").is_err());
        assert!(validate_tag("tag.dot").is_err());
        assert!(validate_tag("tag@at").is_err());
        assert!(validate_tag("tag#hash").is_err());
        assert!(validate_tag("tag$dollar").is_err());
        assert!(validate_tag("tag\"quote").is_err());
    }

    #[test]
    fn validate_tag_rejects_query_injection_attempts() {
        // Attempts to inject ICM query syntax
        assert!(validate_tag("tag,malicious").is_err());
        assert!(validate_tag("tag OR other").is_err());
        assert!(validate_tag("tag AND other").is_err());
    }

    #[test]
    fn dispatch_tag_and_bundle_with_same_name_produce_distinct_recalls() {
        // A name valid as both a tag and a bundle must produce two separate
        // recalls keyed on different prefixes — no cross-contamination.
        let tag_qs = tag_recall_queries(&["foo".to_string()]).expect("valid");
        let bundle_qs = bundle_recall_queries(&["foo".to_string()]).expect("valid");
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs, None);
        assert_eq!(actions.len(), 3);
        match &actions[1] {
            Action::RecallTag(q) => assert_eq!(q.keyword, "llmenv-tag:foo"),
            other => panic!("expected RecallTag, got {other:?}"),
        }
        match &actions[2] {
            Action::RecallBundle(q) => assert_eq!(q.keyword, "llmenv-bundle:foo"),
            other => panic!("expected RecallBundle, got {other:?}"),
        }
    }

    #[test]
    fn bundle_recall_queries_validates_bundle_names() {
        assert!(bundle_recall_queries(&["".to_string()]).is_err());
        assert!(bundle_recall_queries(&["bundle:invalid".to_string()]).is_err());
        assert!(bundle_recall_queries(&["bundle space".to_string()]).is_err());
        assert!(bundle_recall_queries(&["bundle/path".to_string()]).is_err());
    }

    #[test]
    fn validate_bundle_rejects_empty() {
        assert!(validate_bundle("").is_err());
    }

    #[test]
    fn validate_bundle_rejects_special_characters() {
        assert!(validate_bundle("bundle:invalid").is_err());
        assert!(validate_bundle("bundle space").is_err());
        assert!(validate_bundle("bundle/path").is_err());
        assert!(validate_bundle("bundle.dot").is_err());
    }

    #[test]
    fn validate_bundle_rejects_query_injection_attempts() {
        assert!(validate_bundle("bundle,malicious").is_err());
        assert!(validate_bundle("bundle OR other").is_err());
        assert!(validate_bundle("bundle AND other").is_err());
    }

    use proptest::prelude::*;

    fn valid_name() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{1,24}"
    }

    fn arb_hook_event() -> impl Strategy<Value = HookEvent> {
        prop_oneof![
            Just(HookEvent::SessionStart),
            Just(HookEvent::TurnStart),
            Just(HookEvent::SessionEnd),
            Just(HookEvent::UserPromptSubmit),
            Just(HookEvent::PreToolUse),
            Just(HookEvent::PostToolUse),
            Just(HookEvent::Notification),
            Just(HookEvent::Stop),
            Just(HookEvent::SubagentStop),
            Just(HookEvent::PreCompact),
        ]
    }

    /// Arbitrary Claude hook stdin payload shapes: present-and-string,
    /// present-and-wrong-type, and absent, for each field `event_content`
    /// reads. Exercises the adversarial/malformed-payload path (#509 item 5).
    fn arb_verbose_payload() -> impl Strategy<Value = serde_json::Value> {
        let field = |key: &'static str| {
            prop_oneof![
                "[a-zA-Z0-9 _-]{0,16}".prop_map(move |s| (key, serde_json::json!(s))),
                Just((key, serde_json::json!(42))),
                Just((key, serde_json::json!({"nested": "object"}))),
                Just((key, serde_json::Value::Null)),
            ]
        };
        prop::collection::vec(
            prop_oneof![
                field("prompt"),
                field("tool_name"),
                field("tool_input"),
                field("tool_response"),
                field("message"),
                field("last_assistant_message"),
                field("trigger"),
            ],
            0..7,
        )
        .prop_map(|pairs| {
            serde_json::Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        })
    }

    proptest! {
        #[test]
        fn prop_verbose_event_display_round_trips_through_from_str(ev in arb_hook_event()) {
            prop_assert_eq!(ev.to_string().parse::<HookEvent>().unwrap(), ev);
        }

        #[test]
        fn prop_verbose_content_never_panics(
            ev in arb_hook_event(),
            payload in arb_verbose_payload(),
        ) {
            let _ = event_content(ev, &payload);
        }
    }

    proptest! {
        // dispatch(TurnStart) always produces [Recall, N×RecallTag, M×RecallBundle]
        // regardless of N and M. This is the ordering invariant.
        #[test]
        fn prop_dispatch_turn_start_ordering(
            tags in proptest::collection::vec(valid_name(), 0..8),
            bundles in proptest::collection::vec(valid_name(), 0..8),
        ) {
            let tag_qs = tag_recall_queries(&tags).expect("valid tags");
            let bundle_qs = bundle_recall_queries(&bundles).expect("valid bundles");
            let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs, None);

            prop_assert_eq!(actions.len(), 1 + tags.len() + bundles.len());
            prop_assert!(matches!(actions[0], Action::Recall));
            for a in &actions[1..=tags.len()] {
                prop_assert!(matches!(a, Action::RecallTag(_)), "expected RecallTag, got {a:?}");
            }
            for a in &actions[1 + tags.len()..] {
                prop_assert!(
                    matches!(a, Action::RecallBundle(_)),
                    "expected RecallBundle, got {a:?}"
                );
            }
        }
    }

    // ===== #1143: MemoryEndpoint::into_url() message formatting =====

    /// Every `into_url()` variant that carries a name list uses `.join(", ")`
    /// unconditionally — shared assertion so the three variant-specific
    /// proptests below don't each restate it (pre-pr-review finding for
    /// #1234).
    fn assert_message_preserves_names(
        msg: &str,
        names: &[String],
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        for name in names {
            if !msg.contains(name.as_str()) {
                return Err(proptest::test_runner::TestCaseError::fail(format!(
                    "message must preserve {name:?}: {msg}"
                )));
            }
        }
        if !names.is_empty() && !msg.contains(&names.join(", ")) {
            return Err(proptest::test_runner::TestCaseError::fail(format!(
                "names must appear joined with \", \": {msg}"
            )));
        }
        Ok(())
    }

    proptest! {
        // NotDeclared's message must preserve every skipped-bundle name and
        // join them with the same separator regardless of list length or the
        // exact (valid) characters in each name.
        #[test]
        fn prop_notdeclared_message_preserves_every_skipped_bundle_name(
            names in proptest::collection::vec(valid_name(), 0..8),
        ) {
            let msg = MemoryEndpoint::NotDeclared { skipped_bundles: names.clone() }
                .into_url()
                .unwrap_err()
                .to_string();
            assert_message_preserves_names(&msg, &names)?;
        }

        #[test]
        fn prop_suppressed_message_preserves_every_bundle_name(
            names in proptest::collection::vec(valid_name(), 1..8),
        ) {
            let msg = MemoryEndpoint::SuppressedByDisableBundles(names.clone())
                .into_url()
                .unwrap_err()
                .to_string();
            assert_message_preserves_names(&msg, &names)?;
        }

        #[test]
        fn prop_tag_inactive_message_preserves_every_server_host(
            hosts in proptest::collection::vec(valid_name(), 1..8),
        ) {
            let msg = MemoryEndpoint::TagInactive { server_hosts: hosts.clone() }
                .into_url()
                .unwrap_err()
                .to_string();
            assert_message_preserves_names(&msg, &hosts)?;
        }
    }

    // ===== #1143: classify_missing_memory() classification over combinatorial state =====

    /// A minimal but fully-populated `Memory` entry naming `server_host` —
    /// `classify_missing_memory` only reads `server_host` off `all_memory`,
    /// but the struct has no `Default` shorthand for the rest.
    fn memory_with_host(server_host: &str) -> crate::config::Memory {
        crate::config::Memory {
            server_host: server_host.to_string(),
            port: 7878,
            listen_host: "127.0.0.1".into(),
            when: vec![],
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: std::collections::BTreeMap::new(),
            retention: None,
            auto_prune: false,
            consolidation: None,
            mcp_permissions: None,
            wakeup_max_tokens: None,
        }
    }

    proptest! {
        // A non-empty `all_memory` yields `TagInactive` naming every entry's
        // `server_host` (deduped) whenever nothing is suppressed and every
        // firing bundle loaded cleanly — regardless of how many bundles
        // fired. `TagInactive` outranks `NoBundlesFired` (this test) but is
        // itself outranked by a skipped firing bundle (the test below).
        #[test]
        fn prop_classify_missing_memory_tag_inactive_wins_regardless_of_firing(
            server_hosts in proptest::collection::vec(valid_name(), 1..5),
            firing_names in proptest::collection::vec(valid_name(), 0..5),
        ) {
            let config = crate::config::Config::default();
            let config_dir = std::path::Path::new("/nonexistent");
            let active = crate::scope::ActiveScopes::default();
            let bundles: Vec<crate::config::Bundle> = firing_names
                .iter()
                .map(|n| crate::config::Bundle { name: n.clone(), when: vec![] })
                .collect();
            let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
            // Every firing bundle "loads" successfully (a matching `BundleRef`
            // for each name), so `skipped_bundles` stays empty and this
            // isolates the `TagInactive`-vs-`NoBundlesFired` priority — a
            // skipped bundle outranks `TagInactive` (see the test below), so
            // it must not leak into this one.
            let bundle_refs: Vec<crate::merge::BundleRef> = firing_names
                .iter()
                .map(|n| crate::merge::BundleRef {
                    name: n.clone(),
                    path: std::path::PathBuf::new(),
                    precedence: 0,
                })
                .collect();
            let all_memory: Vec<crate::config::Memory> =
                server_hosts.iter().map(|h| memory_with_host(h)).collect();

            let result = classify_missing_memory(
                &config,
                config_dir,
                &active,
                &firing,
                &bundle_refs,
                &all_memory,
            );
            // `classify_missing_memory` dedups `server_hosts` (two entries can
            // legitimately share a `server_host`), so compare against the same
            // deduped/sorted form rather than `server_hosts` as generated.
            let expected: Vec<String> = server_hosts
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            prop_assert_eq!(
                result,
                MemoryEndpoint::TagInactive { server_hosts: expected }
            );
        }

        // A firing bundle `build_bundle_refs` couldn't load outranks a
        // declared-but-tag-inactive entry: the skipped bundle is a real
        // misconfiguration (its own `features.memory`, if any, was never
        // read), while a tag-inactive entry is often intentional.
        #[test]
        fn prop_classify_missing_memory_skipped_bundle_outranks_tag_inactive(
            server_hosts in proptest::collection::vec(valid_name(), 1..5),
            skipped_name in valid_name(),
        ) {
            let config = crate::config::Config::default();
            let config_dir = std::path::Path::new("/nonexistent");
            let active = crate::scope::ActiveScopes::default();
            let bundles = [crate::config::Bundle { name: skipped_name.clone(), when: vec![] }];
            let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
            let all_memory: Vec<crate::config::Memory> =
                server_hosts.iter().map(|h| memory_with_host(h)).collect();

            // No bundle_refs at all: `skipped_name` is firing but unloaded.
            let result =
                classify_missing_memory(&config, config_dir, &active, &firing, &[], &all_memory);
            prop_assert_eq!(
                result,
                MemoryEndpoint::NotDeclared { skipped_bundles: vec![skipped_name] }
            );
        }

        // With `all_memory` empty and nothing suppressed: `NoBundlesFired` iff
        // firing is empty; otherwise `NotDeclared`'s `skipped_bundles` is
        // exactly `firing` filtered down to the names `bundle_refs` didn't
        // load — matching production's `firing \ loaded` computation
        // (duplicates and order included, since production filters the
        // `firing` list itself rather than deduplicating first).
        #[test]
        fn prop_classify_missing_memory_skipped_bundles_is_firing_minus_loaded(
            firing_with_loaded in proptest::collection::vec((valid_name(), any::<bool>()), 0..6),
        ) {
            let config = crate::config::Config::default();
            let config_dir = std::path::Path::new("/nonexistent");
            let active = crate::scope::ActiveScopes::default();
            let firing_names: Vec<String> =
                firing_with_loaded.iter().map(|(n, _)| n.clone()).collect();
            let bundles: Vec<crate::config::Bundle> = firing_names
                .iter()
                .map(|n| crate::config::Bundle { name: n.clone(), when: vec![] })
                .collect();
            let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
            let loaded_names: std::collections::HashSet<&str> = firing_with_loaded
                .iter()
                .filter(|(_, keep)| *keep)
                .map(|(n, _)| n.as_str())
                .collect();
            let bundle_refs: Vec<crate::merge::BundleRef> = loaded_names
                .iter()
                .map(|n| crate::merge::BundleRef {
                    name: (*n).to_string(),
                    path: std::path::PathBuf::new(),
                    precedence: 0,
                })
                .collect();

            let result =
                classify_missing_memory(&config, config_dir, &active, &firing, &bundle_refs, &[]);

            if firing_names.is_empty() {
                prop_assert_eq!(result, MemoryEndpoint::NoBundlesFired);
            } else {
                let MemoryEndpoint::NotDeclared { skipped_bundles } = result else {
                    return Err(proptest::test_runner::TestCaseError::fail(format!(
                        "expected NotDeclared for non-empty firing, got {result:?}"
                    )));
                };
                // Independent membership invariants rather than recomputing
                // production's `firing \ loaded` formula (#1141 pre-pr-review
                // finding): every skipped name is a firing bundle that wasn't
                // loaded, and every unloaded firing bundle is reported.
                for name in &skipped_bundles {
                    prop_assert!(
                        firing_names.contains(name),
                        "skipped name {name:?} must be one of the firing bundles"
                    );
                    prop_assert!(
                        !loaded_names.contains(name.as_str()),
                        "skipped name {name:?} must not be among the loaded bundle_refs"
                    );
                }
                for name in &firing_names {
                    if !loaded_names.contains(name.as_str()) {
                        prop_assert!(
                            skipped_bundles.contains(name),
                            "unloaded firing bundle {name:?} must appear in skipped_bundles"
                        );
                    }
                }
            }
        }
    }

    // ===== #592: apply_memory_config_defaults idempotence =====

    fn memory_config(default_type: Option<llmenv_config::MemoryType>) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.features = Some(crate::config::Features {
            memory: vec![llmenv_config::Memory {
                server_host: "test-host".into(),
                port: 0,
                listen_host: "127.0.0.1".into(),
                when: vec!["test".into()],
                default_topics: vec![],
                default_type,
                default_importance: None,
                type_importance: Default::default(),
                retention: None,
                auto_prune: false,
                consolidation: None,
                mcp_permissions: None,
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        });
        config
    }

    fn active_with_tag(tag: &str) -> crate::scope::ActiveScopes {
        let mut tags = std::collections::BTreeSet::new();
        tags.insert(tag.to_string());
        crate::scope::ActiveScopes {
            tags,
            scopes: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn apply_memory_defaults_idempotent_no_type() {
        let config = memory_config(None);
        let active = active_with_tag("test");
        let input = "## context\nno markers".to_string();
        let once = apply_memory_config_defaults(input, &config, &active);
        let twice = apply_memory_config_defaults(once.clone(), &config, &active);
        assert_eq!(once, twice, "applying defaults twice must be idempotent");
    }

    #[test]
    fn apply_memory_defaults_adds_type_marker_when_present() {
        let config = memory_config(Some(llmenv_config::MemoryType::Semantic));
        let active = active_with_tag("test");
        let input = "## context".to_string();
        let out = apply_memory_config_defaults(input, &config, &active);
        assert!(
            out.contains("<!-- llmenv-type: semantic -->"),
            "should add semantic type marker: {out}"
        );
    }

    #[test]
    fn apply_memory_defaults_skips_existing_marker() {
        let config = memory_config(Some(llmenv_config::MemoryType::Semantic));
        let active = active_with_tag("test");
        let input = "## context\n<!-- llmenv-type: episodic -->".to_string();
        let out = apply_memory_config_defaults(input, &config, &active);
        assert!(
            !out.contains("semantic"),
            "must not override existing episodic marker"
        );
        assert!(
            out.contains("episodic"),
            "existing marker must survive: {out}"
        );
    }

    #[test]
    fn web_fetch_store_args_extracts_url_and_summary() {
        let payload = json!({
            "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"},
            "tool_response": "# Hello\n\nThis is fetched content",
        });
        let args = web_fetch_store_args(&payload).expect("should detect WebFetch");
        assert_eq!(args["topic"], "web-fetch");
        assert_eq!(args["importance"], "low");
        let content = args["content"].as_str().unwrap();
        assert!(content.contains("https://example.com"), "url in content");
        assert!(content.contains("WebFetch"), "tool name in content");
        assert!(
            content.contains("Fetched at (epoch)"),
            "timestamp in content"
        );
        assert!(content.contains("Hello"), "content preview in content");
    }

    #[test]
    fn web_fetch_store_args_supports_web_search() {
        let payload = json!({
            "tool_name": "WebSearch",
            "tool_input": {"query": "rust programming"},
            "tool_response": "Search results here",
        });
        let args = web_fetch_store_args(&payload).expect("should detect WebSearch");
        assert_eq!(args["topic"], "web-fetch");
        let content = args["content"].as_str().unwrap();
        assert!(content.contains("WebSearch"));
        assert!(content.contains("Search results"));
        assert!(content.contains("Query: rust programming"));
        assert!(content.contains("Tool: WebSearch"));
    }

    #[test]
    fn web_fetch_store_args_ignores_non_web_fetch_tools() {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_response": "result",
        });
        assert!(
            web_fetch_store_args(&payload).is_none(),
            "non-WebFetch tool should return None"
        );
    }

    #[test]
    fn web_fetch_store_args_handles_missing_url_or_tool_input() {
        // Missing url key within tool_input.
        let payload = json!({
            "tool_name": "WebFetch",
            "tool_input": {},
            "tool_response": "content",
        });
        let args = web_fetch_store_args(&payload).expect("should handle missing url");
        let content = args["content"].as_str().unwrap();
        assert!(content.contains("unknown"), "should fall back to 'unknown'");

        // Missing entire tool_input key — same serde_json Null path.
        let payload2 = json!({
            "tool_name": "WebFetch",
            "tool_response": "content",
        });
        let args2 = web_fetch_store_args(&payload2).expect("should handle missing tool_input");
        let content2 = args2["content"].as_str().unwrap();
        assert!(
            content2.contains("unknown"),
            "should fall back to 'unknown'"
        );
    }

    #[test]
    fn web_fetch_store_args_handles_empty_response() {
        let payload = json!({
            "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"},
            "tool_response": "",
        });
        let args = web_fetch_store_args(&payload).expect("should handle empty response");
        let content = args["content"].as_str().unwrap();
        assert!(
            content.contains("Content preview:\n"),
            "empty content after preview header"
        );
    }

    #[test]
    fn web_fetch_store_args_truncates_long_content() {
        let long = "x".repeat(2000);
        let payload = json!({
            "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"},
            "tool_response": long,
        });
        let args = web_fetch_store_args(&payload).expect("should handle long content");
        let content = args["content"].as_str().unwrap();
        let preview = content.split("Content preview:\n").nth(1).unwrap_or("");
        assert!(
            preview.ends_with("... (truncated)"),
            "truncation indicator should be present, got: {preview:?}"
        );
        let truncated = preview.strip_suffix("... (truncated)").unwrap_or(preview);
        assert!(
            truncated.len() <= 1000,
            "truncated content should be at most 1000 chars, got {}",
            truncated.len()
        );
    }

    #[test]
    fn web_fetch_store_args_returns_none_for_null_payload() {
        assert!(web_fetch_store_args(&serde_json::Value::Null).is_none());
    }

    #[test]
    fn web_fetch_store_args_returns_none_for_missing_tool_name() {
        let payload = json!({
            "tool_input": {"url": "https://example.com"},
            "tool_response": "content",
        });
        assert!(web_fetch_store_args(&payload).is_none());
    }

    #[test]
    fn web_fetch_store_args_handles_object_tool_response() {
        let payload = json!({
            "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"},
            "tool_response": {"content": [{"type": "text", "text": "hello world"}]},
        });
        let args = web_fetch_store_args(&payload).expect("should handle object response");
        let content = args["content"].as_str().unwrap();
        assert!(
            content.contains("hello world"),
            "extracted text from object response"
        );
    }

    #[test]
    fn handle_web_fetch_post_tool_use_does_not_block() {
        let payload = serde_json::json!({
            "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"},
            "tool_response": "fetched content",
        });
        // The child process (re-invoking the current, test-harness executable
        // with args it doesn't understand) is expected to exit non-zero almost
        // instantly; the parent never waits on it, so this call itself must
        // return promptly.
        let start = std::time::Instant::now();
        let child = handle_web_fetch_post_tool_use(&payload);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "handle_web_fetch_post_tool_use must not block on the child"
        );
        // A well-formed WebFetch payload must actually spawn a child —
        // this is the one assertion that would catch the whole function
        // being replaced with an unconditional `None` (#1465).
        let mut child = child.expect("a well-formed WebFetch payload must spawn a detached child");
        // Reap: production deliberately never waits (the child is
        // process-group-detached), but this test process is still its OS
        // parent — leaving it un-waited leaks a zombie for the rest of the
        // cargo-test run (#1095).
        reap_test_child(&mut child, std::time::Duration::from_secs(5));
    }

    #[test]
    fn post_session_consolidation_spawns_a_child() {
        // `current_exe()` always resolves under the test harness, so this
        // must spawn — the one assertion that would catch the whole
        // function being replaced with an unconditional `None` (#1465).
        let start = std::time::Instant::now();
        let child = post_session_consolidation();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "post_session_consolidation must not block on the child"
        );
        let mut child =
            child.expect("current_exe() resolving must spawn a detached consolidation child");
        reap_test_child(&mut child, std::time::Duration::from_secs(5));
    }

    #[test]
    fn is_post_session_consolidation_event_fires_only_for_post_session() {
        assert!(is_post_session_consolidation_event(HookEvent::PostSession));
        for other in [
            HookEvent::SessionStart,
            HookEvent::TurnStart,
            HookEvent::SessionEnd,
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::Notification,
            HookEvent::Stop,
            HookEvent::SubagentStop,
            HookEvent::PreCompact,
        ] {
            assert!(
                !is_post_session_consolidation_event(other),
                "{other:?} must not trigger post-session consolidation"
            );
        }
    }

    /// Wait for `child` to exit, bounded by `timeout`; force-kill and wait
    /// again if it doesn't exit in time. Used only to keep test-spawned
    /// children from outliving the test run (#1095).
    fn reap_test_child(child: &mut std::process::Child, timeout: std::time::Duration) {
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if start.elapsed() < timeout => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod session_log_tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx() -> ScopeContext {
        ScopeContext {
            tags: vec!["rust".into()],
            bundles: vec![],
            project: Some("llmenv".into()),
            cwd: "/tmp".into(),
            adapter: "claude-code".into(),
            llmenv_version: "3.0.0".into(),
            claude_code_version: String::new(),
        }
    }

    fn file_only_cfg(path: &std::path::Path) -> SessionLog {
        SessionLog {
            file: Some(llmenv_config::FileSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                path: Some(path.to_string_lossy().into_owned()),
            }),
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: false,
                level: LogLevel::Info,
                retention_days: None,
            }),
            max_content_bytes: None,
        }
    }

    fn jsonl_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn session_start_file_only_writes_lifecycle_and_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        handle_session_log(
            HookEvent::SessionStart,
            &file_only_cfg(&path),
            None,
            None,
            &ctx(),
            None,
        )
        .await;
        let lines = jsonl_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "lifecycle_start");
        assert_eq!(lines[1]["kind"], "scope");
        assert!(
            lines[1]["content"]
                .as_str()
                .unwrap()
                .contains("llmenv-tag:rust")
        );
    }

    #[tokio::test]
    async fn session_end_file_only_writes_lifecycle_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        handle_session_log(
            HookEvent::SessionEnd,
            &file_only_cfg(&path),
            None,
            None,
            &ctx(),
            None,
        )
        .await;
        let lines = jsonl_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["kind"], "lifecycle_end");
    }

    #[tokio::test]
    async fn disabled_sinks_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        let cfg = SessionLog {
            file: Some(llmenv_config::FileSinkConfig {
                enabled: false,
                level: LogLevel::Info,
                path: Some(path.to_string_lossy().into_owned()),
            }),
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: false,
                level: LogLevel::Info,
                retention_days: None,
            }),
            max_content_bytes: None,
        };
        handle_session_log(HookEvent::SessionStart, &cfg, None, None, &ctx(), None).await;
        assert!(!path.exists());
    }

    fn mock_text_response(text: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":text}]}})
    }

    // These two test `ensure_transcript_session` directly rather than through
    // `handle_session_log`/`emit_session_log`: since T11, the transcript
    // *record* path dispatches via a detached child process
    // (`session_log::detached::spawn_record`), which a unit test must not
    // trigger (the test binary is not the `llmenv` binary `spawn_record`
    // expects to re-invoke). `start_session` stays synchronous/inline
    // (`ensure_transcript_session`), so it remains directly unit-testable.

    #[tokio::test]
    async fn ensure_transcript_session_creates_and_correlates_when_none_recorded() {
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(mock_text_response("icm-sess-1")),
            )
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let cfg = SessionLog {
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                retention_days: None,
            }),
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id = ensure_transcript_session(
            &cfg,
            Some(&client),
            "claude-1",
            &ctx(),
            Some(&state_path),
            true,
        )
        .await;

        assert_eq!(id.as_deref(), Some("icm-sess-1"));
        assert_eq!(
            state::lookup_session_at(&state_path, "claude-1").as_deref(),
            Some("icm-sess-1")
        );
    }

    #[tokio::test]
    async fn ensure_transcript_session_reuses_existing_after_verifying_it_is_live() {
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        state::record_session_at(&state_path, "claude-2", "icm-sess-2").unwrap();
        let server = MockServer::start().await;
        // Only `initialize` and the `icm_transcript_show` verification call
        // are mocked. If `ensure_transcript_session` fell through to
        // `start_session` instead of trusting a verified cached id, that
        // call would hit an unmocked request and the id assertion below
        // would fail.
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                crate::session_log::transcript::SHOW_TOOL,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("[]")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let cfg = SessionLog {
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                retention_days: None,
            }),
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id = ensure_transcript_session(
            &cfg,
            Some(&client),
            "claude-2",
            &ctx(),
            Some(&state_path),
            true,
        )
        .await;

        assert_eq!(id.as_deref(), Some("icm-sess-2"));
    }

    /// #1090 regression: `run_session_log`'s per-event path calls
    /// `ensure_transcript_session` for every mapped hook event, not just
    /// `SessionStart` — verifying on every one of those would turn each
    /// logged tool call into an `icm_transcript_show` round trip that grows
    /// with the transcript itself (`icm_transcript_show` has no cheap
    /// existence-only form). `verify: false` must reuse a cached id without
    /// ever calling ICM.
    #[tokio::test]
    async fn ensure_transcript_session_with_verify_false_skips_the_network_call() {
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        state::record_session_at(&state_path, "claude-5", "icm-sess-5").unwrap();
        // No mock mounted at all: any HTTP call here would fail the request.
        let server = MockServer::start().await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let cfg = SessionLog {
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                retention_days: None,
            }),
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id = ensure_transcript_session(
            &cfg,
            Some(&client),
            "claude-5",
            &ctx(),
            Some(&state_path),
            false,
        )
        .await;

        assert_eq!(id.as_deref(), Some("icm-sess-5"));
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "verify: false must not make any ICM call"
        );
    }

    #[tokio::test]
    async fn ensure_transcript_session_reestablishes_when_cached_id_fails_verification() {
        // #1090: a cached icm_session_id must be revalidated against ICM, not
        // trusted forever — a stale one is cleared and replaced.
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        state::record_session_at(&state_path, "claude-4", "icm-stale").unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                crate::session_log::transcript::SHOW_TOOL,
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                crate::session_log::transcript::START_TOOL,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-fresh")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let cfg = SessionLog {
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                retention_days: None,
            }),
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id = ensure_transcript_session(
            &cfg,
            Some(&client),
            "claude-4",
            &ctx(),
            Some(&state_path),
            true,
        )
        .await;

        assert_eq!(
            id.as_deref(),
            Some("icm-fresh"),
            "a cached id that fails verification must be replaced"
        );
        assert_eq!(
            state::lookup_session_at(&state_path, "claude-4").as_deref(),
            Some("icm-fresh"),
            "the correlation map must be updated to the fresh id"
        );
    }

    #[tokio::test]
    async fn handle_session_log_session_end_reuses_correlated_session_id() {
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        state::record_session_at(&state_path, "claude-3", "icm-sess-3").unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        let path = log_dir.path().join("session-log.jsonl");
        // transcript: false here only to avoid the detached-spawn side effect
        // in emit_session_log; the lookup itself (asserted via the return
        // value) doesn't depend on cfg.transcript.
        let cfg = file_only_cfg(&path);

        let id = handle_session_log(
            HookEvent::SessionEnd,
            &cfg,
            None,
            Some("claude-3"),
            &ctx(),
            Some(&state_path),
        )
        .await;

        assert_eq!(id.as_deref(), Some("icm-sess-3"));
        let lines = jsonl_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["kind"], "lifecycle_end");
    }
}
