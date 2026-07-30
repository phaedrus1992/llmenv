//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

pub(crate) mod action;
pub(crate) mod detached_consolidation;
pub(crate) mod detached_store;
pub(crate) mod mcp_client;
pub(crate) mod read_once;
pub(crate) mod repeat_detect;
mod session_state;
pub(crate) mod task_tools;

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
/// the single sources of tag→recall and bundle→recall expansion).
///
/// `TurnStart` runs the project-scoped natural-language `Recall` first, then one
/// project-unfiltered `RecallTag` per active tag (#197), then one
/// project-unfiltered `RecallBundle` per active bundle (#228). The turn-capture
/// events carry no memory actions.
fn dispatch(
    event: HookEvent,
    tag_queries: &[TagRecallQuery],
    bundle_queries: &[BundleRecallQuery],
) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => vec![Action::WakeUp],
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

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(())` even when the backend is unreachable.
pub fn run(event: &str, engine: &str) -> anyhow::Result<()> {
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
            return Ok(());
        }
    };
    let null_payload = serde_json::Value::Null;
    let payload = stdin_json.as_ref().unwrap_or(&null_payload);
    let adapter = crate::adapter::adapter_for_engine(engine);
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
                        "deniedReason": reason,
                    }
                });
                if let Err(e) = writeln!(std::io::stdout(), "{envelope}")
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("llmenv: failed to write hook output: {e}");
                }
            } else {
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
    Ok(())
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
pub(crate) fn reset_preloaded_config_for_test() {
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
fn resolve_pre_tool_text(
    stdin_payload: &serde_json::Value,
    claude_session_id: Option<&str>,
    config: &crate::config::Config,
    task_tracker_enabled: bool,
    state_dir: &std::path::Path,
) -> Option<String> {
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

    match (primary, repeat_detect_text) {
        (Some(p), Some(r)) => Some(format!("{p}\n\n{r}")),
        (Some(p), None) => Some(p),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    }
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
    let reminder = crate::task::stop_hook_reminder(state_dir);
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
        // A `state_dir()` failure must degrade the same way it did before
        // #1089 (each of read_once/repeat_detect resolved it independently
        // and skipped itself on error) rather than propagate via `?` and
        // abort the whole PreToolUse decision — that would silently drop
        // the task-tracker redirect too, and any session logging below,
        // reintroducing the #231/#864 early-return-drops-logging bug class
        // for a failure mode neither of those issues anticipated.
        let text = match crate::paths::state_dir() {
            Ok(state_dir) => resolve_pre_tool_text(
                stdin_payload,
                claude_session_id,
                &config,
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
                    .then(|| {
                        crate::hook_run::task_tools::deny_tracker_unavailable(stdin_payload, &e)
                    })
                    .flatten()
            }
        };
        match text {
            Some(t) => {
                // Derived from the same `event_to_log_kind` mapping
                // `run_session_log` uses, rather than hardcoding `LogLevel::Debug`
                // — a hardcoded level would drift if `EventKind::ToolUse`'s level
                // ever changed, reintroducing this exact bug class.
                let level =
                    event_to_log_kind(event).map_or(LogLevel::Debug, |(kind, _)| kind.log_level());
                if !log_cfg.any_sink_wants(level) {
                    return Ok(t);
                }
                Some(t)
            }
            None => None,
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
        return Ok(resolve_stop_reminder(
            &state_dir,
            claude_session_id,
            &config,
        ));
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

        // Reuse MCP HTTP client across events: the memory backend URL doesn't
        // change mid-session, so the reqwest Client (connection pool, TLS state,
        // DNS cache) is only built once. Cloning the cached McpHttpClient is
        // cheap — reqwest::Client is internally Arc, and the MCP session_id is
        // shared via Arc so re-initialization is also avoided.
        static MCP_CLIENT_CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();
        // No backend is not fatal: memory actions are simply skipped below, but
        // session logging (independent of the memory backend) still proceeds.
        // `into_url` names *which* of the inactive states applies (#1131), so
        // the user isn't sent to read their scope config when the cause is a
        // `disable_bundles` entry or a bundle with no content directory.
        let client: Option<McpHttpClient> = match memory_url(&config, config_dir, &active)?
            .into_url()
        {
            Err(e) => {
                eprintln!("llmenv: memory {event} skipped: {e}");
                None
            }
            Ok(u) => {
                let clients = MCP_CLIENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
                let mut clients = clients.lock().unwrap_or_else(|e| e.into_inner());
                match clients.entry(u) {
                    std::collections::hash_map::Entry::Occupied(entry) => Some(entry.get().clone()),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        match McpHttpClient::new(entry.key().clone(), HOOK_TIMEOUT) {
                            Ok(client) => Some(entry.insert(client).clone()),
                            Err(e) => {
                                eprintln!(
                                    "llmenv: memory {event} skipped: invalid memory backend URL: {e}"
                                );
                                None
                            }
                        }
                    }
                }
            }
        };
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
                let actions = dispatch(event, &tag_queries, &bundle_queries);
                out = run_memory_actions(client, actions, &query, &chunk).await?;

                // PostSession: run reflective consolidation (R5) in a detached
                // child process so the hook returns immediately instead of
                // blocking on MCP. The result is fire-and-forget — PostSession is
                // the final event, so no caller needs its output.
                if event == HookEvent::PostSession {
                    post_session_consolidation();
                }

                // PostToolUse WebFetch/WebSearch: auto-store fetched content in ICM
                // with fast-falloff memory (topic: web-fetch, importance: low) so it
                // survives session compactions but decays quickly. (#579)
                if event == HookEvent::PostToolUse {
                    handle_web_fetch_post_tool_use(stdin_payload);
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

            Ok::<String, anyhow::Error>(out)
        })?;
        let t_end = std::time::Instant::now();

        // Per-phase timing marker. When `LLMENV_TRACE_TIMING` is set (any value) we
        // emit exactly ONE line to stderr:
        //   llmenv-trace {"config_load_us":N,"scope_eval_us":N,"prep_us":N,"mcp_us":N}
        // `prep_us` spans t_scope→t_chunk: recall-query building, context-chunk
        // generation, MCP client construction (reqwest/TLS on a cache miss), the
        // scope-context build, and the one-time ~3ms tokio runtime build — i.e. all
        // setup before the async MCP round-trips. `mcp_us` is the `block_on` window:
        // the round-trips plus session logging. The clock always runs (Instant::now
        // is ~20ns); only emission is gated, so normal runs are unaffected and stdout
        // is never touched. Events that early-return, and runs that error before this
        // point (e.g. a failed MCP round-trip), emit nothing.
        if std::env::var_os("LLMENV_TRACE_TIMING").is_some() {
            // Cap rather than panic on the (unreachable) overflow of an in-process
            // Instant delta past u64::MAX microseconds (~585,000 years).
            let us = |d: std::time::Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
            eprintln!(
                "llmenv-trace {}",
                json!({
                    "config_load_us": us(t_config.saturating_duration_since(t0)),
                    "scope_eval_us": us(t_scope.saturating_duration_since(t_config)),
                    "prep_us": us(t_chunk.saturating_duration_since(t_scope)),
                    "mcp_us": us(t_end.saturating_duration_since(t_chunk)),
                })
            );
        }
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
    let mut kept: Vec<String> = Vec::new();
    for action in actions {
        let text = action.run(client, query, chunk).await?;
        if text.is_empty() || kept.contains(&text) {
            continue;
        }
        kept.push(text);
    }
    Ok(kept.join("\n\n"))
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
        crate::session_log::detached::spawn_record(sid, &ev);
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
/// Replaces the `Option<String>` that collapsed four distinguishable states
/// into `None` (#1131/#1132): a caller could not tell a project that simply
/// declares no memory from one whose only memory-carrying bundle is suppressed
/// by `disable_bundles`. The fourth state — a failed bundle merge — is an `Err`
/// from [`memory_url`] rather than a variant here, because a backend may well
/// be configured and merely unparseable: that is a failure, not an absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryEndpoint {
    /// The memory backend resolved to this HTTP URL.
    Active(String),
    /// No bundle fired for the active scopes and no top-level `features.memory`
    /// entry matched — nothing could have supplied a backend.
    NoBundlesFired,
    /// Bundles fired, but neither they nor the top-level config declare a
    /// `features.memory` entry active for these tags. `skipped_bundles` names
    /// firing bundles that `build_bundle_refs` dropped for having no content
    /// directory, so their `bundle.yaml` was never read (#1133).
    NotDeclared { skipped_bundles: Vec<String> },
    /// `features.memory` is supplied only by these bundles, which the active
    /// scopes suppress via `disable_bundles` (#194).
    SuppressedByDisableBundles(Vec<String>),
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
            Self::Active(url) => Ok(url),
            Self::NoBundlesFired => Err(anyhow::anyhow!(
                "{PREFIX}: no bundles fired and config.yaml declares no features.memory"
            )),
            Self::NotDeclared { skipped_bundles } if skipped_bundles.is_empty() => {
                Err(anyhow::anyhow!(
                    "{PREFIX}: no active bundle or config.yaml declares features.memory"
                ))
            }
            Self::NotDeclared { skipped_bundles } => Err(anyhow::anyhow!(
                "{PREFIX}: bundle(s) {} fired but have no content directory under \
                 the config dir's bundles/, so any features.memory they declare \
                 was never loaded",
                skipped_bundles.join(", ")
            )),
            Self::SuppressedByDisableBundles(names) => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory is supplied only by bundle(s) {}, which \
                 this project turns off via disable_bundles",
                names.join(", ")
            )),
        }
    }
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
    let url = resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some(url),
        _ => None,
    });
    Ok(match url {
        Some(url) => MemoryEndpoint::Active(url),
        None => classify_missing_memory(config, config_dir, active, &firing, &bundle_refs),
    })
}

/// Explain why no memory endpoint resolved (#1131).
///
/// Only reached once resolution has already come up empty, so the extra
/// `bundle.yaml` reads it does to attribute a cause stay off the hot path that
/// every hook event takes.
fn classify_missing_memory(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    firing: &[&crate::config::Bundle],
    bundle_refs: &[crate::merge::BundleRef],
) -> MemoryEndpoint {
    let suppressed: Vec<String> = suppressed_bundle_capabilities(config, config_dir, active)
        .into_iter()
        .filter(|(_, caps)| caps.features.as_ref().is_some_and(|f| !f.memory.is_empty()))
        .map(|(name, _)| name)
        .collect();
    if !suppressed.is_empty() {
        return MemoryEndpoint::SuppressedByDisableBundles(suppressed);
    }
    if firing.is_empty() {
        return MemoryEndpoint::NoBundlesFired;
    }
    let loaded: std::collections::HashSet<&str> =
        bundle_refs.iter().map(|r| r.name.as_str()).collect();
    MemoryEndpoint::NotDeclared {
        skipped_bundles: firing
            .iter()
            .map(|b| b.name.as_str())
            .filter(|n| !loaded.contains(n))
            .map(str::to_owned)
            .collect(),
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
pub(crate) fn suppressed_bundle_capabilities(
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
        .filter(|b| {
            b.when.iter().any(|t| active.tags.contains(t)) || manually_enabled.contains(&b.name)
        })
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
            tracing::warn!("failed to compute merge signature for cache lookup: {e}");
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
fn handle_web_fetch_post_tool_use(payload: &serde_json::Value) {
    let Some(args) = web_fetch_store_args(payload) else {
        return;
    };
    let Ok(payload_json) = serde_json::to_string(&args) else {
        tracing::debug!("icm-store: failed to serialize store args");
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("icm-store: cannot resolve current_exe for detached store");
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("icm-store")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null());
    redirect_stderr_to_detached_log(&mut cmd);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::debug!("icm-store: failed to spawn detached store child");
        return;
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        tracing::debug!("icm-store: failed to pipe args to detached child: {e}");
    }
    // Not waited on: the child is process-group-detached and outlives us.
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

/// Rotation bound for the indexer's diagnostic log — smaller than the
/// mcp-proxy log (#1086/#1091 share the "size-bounded" shape, not the exact
/// size: indexing runs are far less frequent than proxy restarts).
const CODEBASE_MEMORY_LOG_MAX_BYTES: u64 = 1 << 19; // 512 KiB

/// Rotation bound for the detached hook children's shared stderr log. Same
/// size as the indexer log for the same reason: these children run often but
/// write nothing unless they fail.
const DETACHED_CHILD_LOG_MAX_BYTES: u64 = 1 << 19; // 512 KiB

/// Path of the stderr log shared by llmenv's detached hook children —
/// `<state_dir>/detached-hook.log`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
fn detached_child_log_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::state_dir()?.join("detached-hook.log"))
}

/// Point `cmd`'s stderr at `log_path` as a size-bounded diagnostic log.
///
/// If the log can't be opened the child still runs with stderr discarded — a
/// missing diagnostic is a smaller problem than skipping the work.
fn redirect_stderr_to_bounded_log(cmd: &mut std::process::Command, log_path: &std::path::Path) {
    match crate::mcp::proxy::open_bounded_log(log_path, DETACHED_CHILD_LOG_MAX_BYTES) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            tracing::debug!("detached child: log unavailable ({e:#}), stderr discarded");
        }
    }
}

/// Send a detached child's stderr to the shared bounded log instead of
/// discarding it (#1133, the same remedy as #1091).
///
/// `Stdio::null()` leaves such a child with no report channel whatsoever: its
/// own `tracing` events go to a fmt layer writing to that same null stderr, so
/// a failure is discarded twice over.
pub(crate) fn redirect_stderr_to_detached_log(cmd: &mut std::process::Command) {
    match detached_child_log_path() {
        Ok(path) => redirect_stderr_to_bounded_log(cmd, &path),
        Err(e) => {
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
    let log_path = codebase_memory_cache_dir(cm, state_dir).join("index.log");
    match crate::mcp::proxy::open_bounded_log(&log_path, CODEBASE_MEMORY_LOG_MAX_BYTES) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            tracing::debug!(
                "codebase-memory-mcp index_repository: log unavailable ({e:#}), stderr discarded"
            );
        }
    }
    crate::mcp::proxy::detach_process_group(&mut cmd);
    if let Err(e) = cmd.spawn() {
        tracing::debug!("codebase-memory-mcp index_repository: failed to spawn: {e}");
    }
}

/// Spawn a detached child to run post-session consolidation. Best-effort
/// fire-and-forget — spawn failures are logged at debug level and the caller
/// never waits on the child. The child's stderr goes to the shared bounded log
/// rather than `/dev/null` so its own failures are diagnosable (#1133).
fn post_session_consolidation() {
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("consolidation-run: cannot resolve current_exe");
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("consolidation-run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    redirect_stderr_to_detached_log(&mut cmd);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    if let Err(e) = cmd.spawn() {
        tracing::debug!("consolidation-run: failed to spawn detached child: {e}");
    }
    // Not waited on: the child is process-group-detached and outlives us.
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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
            MemoryEndpoint::Active("http://still.local:7878/mcp".into()),
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
            MemoryEndpoint::Active("http://still.local:7878/mcp".into()),
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
        redirect_stderr_to_bounded_log(&mut cmd, &log);

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
        use tracing_subscriber::prelude::*;

        let (config_root, _cache, config, active) = unreadable_bundle_fixture();
        let log_dir = tempfile::tempdir().expect("test");
        let log = log_dir.path().join("events.jsonl");
        let sub = tracing_subscriber::registry().with(
            crate::session_log::tracing_layer::FileLogLayer::new(
                crate::session_log::file_sink::FileSink::new(log.clone()),
            ),
        );
        let result = tracing::subscriber::with_default(sub, || {
            memory_url(&config, config_root.path(), &active)
        });

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
            "a signature failure must be logged before falling back to a live merge: {body}"
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
            !matches!(url, MemoryEndpoint::Active(_)),
            "a bundle disabled via `disable_bundles` must not contribute its \
             memory/host entries to memory_url resolution, got {url:?}"
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
        };
        trigger_codebase_memory_index(std::path::Path::new("/repos/proj"), &cm, state_dir.path());

        let log_path = state_dir.path().join("codebase-memory").join("index.log");
        let meta = std::fs::metadata(&log_path)
            .unwrap_or_else(|e| panic!("expected {} to exist: {e}", log_path.display()));
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
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
            assert_eq!(dispatch(ev, &[], &[]), Vec::<Action>::new());
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
            dispatch(HookEvent::SessionStart, &[], &[]),
            vec![Action::WakeUp]
        );
        assert_eq!(
            dispatch(HookEvent::TurnStart, &[], &[]),
            vec![Action::Recall]
        );
        assert_eq!(
            dispatch(HookEvent::SessionEnd, &[], &[]),
            vec![Action::Store]
        );
        assert_eq!(
            dispatch(HookEvent::PostSession, &[], &[]),
            vec![],
            "PostSession defers to consolidation module, no dispatch actions"
        );
    }

    #[test]
    fn turn_start_expands_one_recall_tag_per_active_tag() {
        let tags = vec!["rust".to_string(), "work-vpn".to_string()];
        let queries = tag_recall_queries(&tags).expect("valid tags");
        let actions = dispatch(HookEvent::TurnStart, &queries, &[]);
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
        let actions = dispatch(HookEvent::TurnStart, &[], &queries);
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
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs);
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
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs);
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
            let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs);

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
        handle_web_fetch_post_tool_use(&payload);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "handle_web_fetch_post_tool_use must not block on the child"
        );
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
