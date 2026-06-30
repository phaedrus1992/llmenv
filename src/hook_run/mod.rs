//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

pub(crate) mod action;
pub(crate) mod mcp_client;

use std::io::Write;
use std::str::FromStr;
use std::time::Duration;

use action::Action;
use mcp_client::McpHttpClient;
use tracing::{debug, warn};

use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::config::SessionLog;
use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::mcp::resolve::{ResolvedKind, resolve_mcps};
use crate::session_log::dispatch as transcript_dispatch;
use crate::session_log::event::{EventKind, EventScope, SessionLogEvent, now_rfc3339};
use crate::session_log::{ScopeContext, scope_header_content, scope_metadata_json, state};

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
/// rest exist purely for `session_log.verbose` capture (see
/// `event_to_log_kind`); they carry no memory actions of their own — Claude's
/// `UserPromptSubmit` native hook fires both `TurnStart` (memory recall) and
/// `UserPromptSubmit` (verbose prompt capture) as two separate handlers on the
/// same event (see adapter wiring, #382 Task 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Session begins (Claude Code: `SessionStart`).
    SessionStart,
    /// A user prompt/turn begins (Claude Code: `UserPromptSubmit`).
    TurnStart,
    /// Session ends (Claude Code: `SessionEnd`).
    SessionEnd,
    /// Verbose: the raw prompt submission (Claude Code: `UserPromptSubmit`).
    UserPromptSubmit,
    /// Verbose: before a tool call (Claude Code: `PreToolUse`).
    PreToolUse,
    /// Verbose: after a tool call (Claude Code: `PostToolUse`).
    PostToolUse,
    /// Verbose: a UI notification fired (Claude Code: `Notification`).
    Notification,
    /// Verbose: the main agent finished responding (Claude Code: `Stop`).
    Stop,
    /// Verbose: a subagent finished responding (Claude Code: `SubagentStop`).
    SubagentStop,
    /// Verbose: about to compact the transcript (Claude Code: `PreCompact`).
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
/// project-unfiltered `RecallBundle` per active bundle (#228). The verbose-only
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
    }
}

/// Maps a verbose `HookEvent` to its session-log `(kind, role)`. `None` for
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
    }
}

/// Extract `(tool_name, content)` for a verbose event from Claude's hook
/// stdin payload. Field names per the Claude Code hooks reference: prompt
/// text on `UserPromptSubmit` is `prompt`; tool calls carry `tool_name` +
/// `tool_input` (`PreToolUse`) or `tool_input` + `tool_response`
/// (`PostToolUse`); `Notification` carries `message`; `Stop`/`SubagentStop`
/// carry `last_assistant_message`; `PreCompact` carries `trigger`.
fn verbose_content(event: HookEvent, payload: &serde_json::Value) -> (Option<String>, String) {
    // String-typed fields (prompt text, messages): "" when absent/non-string.
    let as_str_field =
        |v: &serde_json::Value| -> String { v.as_str().map(str::to_owned).unwrap_or_default() };
    // Object-typed fields (tool input/response): compact JSON, "" when absent.
    let as_json_text = |v: &serde_json::Value| -> String {
        if v.is_null() {
            String::new()
        } else {
            v.to_string()
        }
    };
    match event {
        HookEvent::UserPromptSubmit => (None, as_str_field(&payload["prompt"])),
        HookEvent::PreToolUse => (
            payload["tool_name"].as_str().map(str::to_owned),
            as_json_text(&payload["tool_input"]),
        ),
        HookEvent::PostToolUse => (
            payload["tool_name"].as_str().map(str::to_owned),
            as_json_text(&payload["tool_response"]),
        ),
        HookEvent::Notification => (None, as_str_field(&payload["message"])),
        HookEvent::Stop | HookEvent::SubagentStop => {
            (None, as_str_field(&payload["last_assistant_message"]))
        }
        HookEvent::PreCompact => (None, as_str_field(&payload["trigger"])),
        HookEvent::SessionStart | HookEvent::TurnStart | HookEvent::SessionEnd => {
            (None, String::new())
        }
    }
}

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(())` even when the backend is unreachable.
pub fn run(event: &str) -> anyhow::Result<()> {
    use std::io::Read;

    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv hook-run: failed to read stdin: {e}");
    }
    let stdin_json = serde_json::from_str::<serde_json::Value>(&stdin_buf).ok();
    let hook_event_name = stdin_json
        .as_ref()
        .and_then(|v| v["hook_event_name"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let claude_session_id = stdin_json
        .as_ref()
        .and_then(|v| v["session_id"].as_str().map(str::to_owned));

    let parsed = match HookEvent::from_str(event) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("llmenv: {e}");
            return Ok(());
        }
    };
    let null_payload = serde_json::Value::Null;
    let payload = stdin_json.as_ref().unwrap_or(&null_payload);
    match run_inner(parsed, claude_session_id.as_deref(), payload) {
        Ok(text) => {
            let out = ClaudeCodeAdapter.emit_hook_context(&hook_event_name, &text);
            if !out.is_empty()
                && let Err(e) = writeln!(std::io::stdout(), "{out}")
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                eprintln!("llmenv: failed to write hook output: {e}");
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
fn run_inner(
    event: HookEvent,
    claude_session_id: Option<&str>,
    stdin_payload: &serde_json::Value,
) -> anyhow::Result<String> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let log_cfg = config.session_log_resolved();

    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;

    // Recall query: the sorted active tags. Store content: the llmenv context
    // chunk (tags/bundles/project).
    let mut tags = active.tags.iter().cloned().collect::<Vec<_>>();
    tags.sort();
    // Bundles: collect from all active scopes, deduplicate, sort.
    let bundles: Vec<String> = {
        let mut set = std::collections::BTreeSet::new();
        for scope in &active.scopes {
            for b in &scope.enable_bundles {
                set.insert(b.clone());
            }
        }
        set.into_iter().collect()
    };
    // Build per-tag and per-bundle recall queries. Validation rejects query
    // injection; these are the single sources of the tag/bundle→keyword encoding.
    let tag_queries = tag_recall_queries(&tags)?;
    let bundle_queries = bundle_recall_queries(&bundles)?;
    let query = tags.join(", ");
    let chunk = crate::icm::generate_context_chunk(&active, &bundles);

    let url = memory_url(&config, config_dir, &active)?;
    if url.is_none() {
        // Not fatal: memory actions are simply skipped below, but session
        // logging (independent of the memory backend) still proceeds.
        eprintln!("llmenv: memory {event} skipped: no memory backend active for this scope");
    }
    let client = url
        .map(|u| McpHttpClient::new(u, HOOK_TIMEOUT))
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;
    let state_path = state::state_path()
        .inspect_err(|e| {
            debug!("session_log: cannot resolve state path, correlation disabled: {e}")
        })
        .ok();
    let ctx = build_scope_context(&active, &tags, &bundles, &env.cwd);

    // Current-thread runtime: lifecycle hooks run on the agent's hot path (session
    // start + every prompt turn) and only need to `block_on` a short sequence of
    // HTTP round-trips. A multi-threaded runtime would spin up a worker thread pool
    // that's pure overhead for this single sequential await. (#186)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let session_log = SessionLogCall {
        log_cfg: &log_cfg,
        client: client.as_ref(),
        claude_session_id,
        ctx: &ctx,
        state_path: state_path.as_deref(),
    };
    rt.block_on(async {
        let mut out = String::new();
        if let Some(client) = &client {
            let actions = dispatch(event, &tag_queries, &bundle_queries);
            out = run_memory_actions(client, actions, &query, &chunk).await?;
        }
        run_session_log(event, &session_log, stdin_payload).await;
        Ok::<String, anyhow::Error>(out)
    })
}

/// Run one event's ordered memory actions and concatenate their text output.
async fn run_memory_actions(
    client: &McpHttpClient,
    actions: Vec<Action>,
    query: &str,
    chunk: &str,
) -> anyhow::Result<String> {
    let mut out = String::new();
    for action in actions {
        let text = action.run(client, query, chunk).await?;
        if !text.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
        }
    }
    Ok(out)
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
/// for `SessionStart`/`SessionEnd`, or (when `verbose`) the per-hook capture
/// event for every other mapped event. No-op for unmapped events.
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
    let (true, Some((kind, role))) = (call.log_cfg.verbose, event_to_log_kind(event)) else {
        return;
    };
    let session_id = match call.claude_session_id {
        Some(csid) => {
            ensure_transcript_session(call.log_cfg, call.client, csid, call.ctx, call.state_path)
                .await
        }
        None => {
            debug!("verbose event captured without claude_session_id — transcript record skipped");
            None
        }
    };
    let (tool_name, content) = verbose_content(event, stdin_payload);
    emit_session_log(
        verbose_session_event(kind, role, tool_name, content),
        call.log_cfg,
        session_id.as_deref(),
    );
}

/// Build the active-scope context a session's lifecycle/scope-header events
/// carry. `tags`/`bundles` are the already-sorted/deduplicated sets `run_inner`
/// computed; the project name comes from the first project-kind active scope.
fn build_scope_context(
    active: &crate::scope::ActiveScopes,
    tags: &[String],
    bundles: &[String],
    cwd: &str,
) -> ScopeContext {
    let project = active
        .scopes
        .iter()
        .find(|s| s.kind == "project")
        .and_then(|s| s.name.clone());
    ScopeContext {
        tags: tags.to_vec(),
        bundles: bundles.to_vec(),
        project,
        cwd: cwd.to_string(),
        adapter: ClaudeCodeAdapter.name().to_string(),
        llmenv_version: env!("CARGO_PKG_VERSION").to_string(),
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
    if !(cfg.file || cfg.transcript) {
        return None;
    }
    let session_id = match (event, claude_session_id) {
        (HookEvent::SessionStart, Some(csid)) => {
            ensure_transcript_session(cfg, client, csid, ctx, state_path).await
        }
        (_, Some(csid)) => state_path.and_then(|p| state::lookup_session_at(p, csid)),
        (_, None) => None,
    };
    let Some(lifecycle_kind) = (match event {
        HookEvent::SessionStart => Some(EventKind::LifecycleStart),
        HookEvent::SessionEnd => Some(EventKind::LifecycleEnd),
        _ => None,
    }) else {
        return session_id;
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
async fn ensure_transcript_session(
    cfg: &SessionLog,
    client: Option<&McpHttpClient>,
    csid: &str,
    ctx: &ScopeContext,
    state_path: Option<&std::path::Path>,
) -> Option<String> {
    let path = state_path?;
    if let Some(existing) = state::lookup_session_at(path, csid) {
        return Some(existing);
    }
    let (true, Some(client)) = (cfg.transcript, client) else {
        return None;
    };
    let metadata = scope_metadata_json(ctx);
    match transcript_dispatch::start_session(
        client,
        ClaudeCodeAdapter.name(),
        ctx.project.as_deref(),
        &metadata,
    )
    .await
    {
        Ok(id) => {
            if let Err(e) = state::record_session_at(path, csid, &id) {
                warn!(error = %e, "failed to persist transcript session correlation");
            }
            Some(id)
        }
        Err(e) => {
            warn!(error = %e, "failed to start ICM transcript session");
            None
        }
    }
}

/// Append `ev` to the configured sinks: the JSONL file (if `cfg.file`,
/// written synchronously) and, for agent-session-scoped events, the ICM
/// transcript (if `cfg.transcript` and a transcript session id is known —
/// dispatched via a detached child, see `session_log::detached`, so this
/// never blocks on the network). Fail-soft.
fn emit_session_log(ev: SessionLogEvent, cfg: &SessionLog, session_id: Option<&str>) {
    let max = cfg.max_content_bytes.unwrap_or(16_384);
    let ev = ev.truncated(max);
    if cfg.file {
        let path = cfg
            .path
            .clone()
            .map(std::path::PathBuf::from)
            .or_else(|| crate::session_log::default_file_path().ok());
        if let Some(p) = path {
            crate::session_log::FileSink::new(p).append(&ev.to_jsonl());
        }
    }
    if cfg.transcript
        && ev.scope == EventScope::AgentSession
        && let Some(sid) = session_id
    {
        crate::session_log::detached::spawn_record(sid, &ev);
    }
}

fn lifecycle_session_event(kind: EventKind, content: &str) -> SessionLogEvent {
    SessionLogEvent {
        ts: now_rfc3339(),
        kind,
        scope: EventScope::AgentSession,
        role: "system".into(),
        tool_name: None,
        tokens: None,
        level: None,
        content: content.to_string(),
        fields: serde_json::json!({}),
    }
}

fn scope_session_event(ctx: &ScopeContext) -> SessionLogEvent {
    SessionLogEvent {
        ts: now_rfc3339(),
        kind: EventKind::Scope,
        scope: EventScope::AgentSession,
        role: "system".into(),
        tool_name: None,
        tokens: None,
        level: None,
        content: scope_header_content(ctx),
        fields: scope_metadata_json(ctx),
    }
}

/// Build a verbose-capture event (`session_log.verbose`) from a Claude hook
/// payload, as extracted by `verbose_content`.
fn verbose_session_event(
    kind: EventKind,
    role: &str,
    tool_name: Option<String>,
    content: String,
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
        fields: serde_json::json!({}),
    }
}

/// Find the resolved memory backend's HTTP URL for the active tags, if any.
///
/// Mirrors the `build_manifest` merge strategy: top-level config memory is
/// combined with bundle-contributed memory entries so a daemon declared only
/// in a `bundle.yaml` is reachable from lifecycle hooks.
///
/// `pub(crate)`: also called by `session_log::detached::run_record`, the
/// detached transcript-record child, which re-resolves the same MCP endpoint
/// independently rather than receiving it as a (process-list-visible) CLI arg.
pub(crate) fn memory_url(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<Option<String>> {
    let top_memory = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();

    // Collect bundle-contributed memory and host entries.
    let manually_enabled: std::collections::BTreeSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&crate::config::Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            b.when.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    let bundle_refs = build_hook_bundle_refs(config_dir, &firing);
    let (bundle_memory, bundle_host) = if bundle_refs.is_empty() {
        (Vec::new(), std::collections::BTreeMap::new())
    } else {
        let merged = crate::merge::merge(&config.capabilities, &config.native, &bundle_refs)
            .unwrap_or_default();
        let mem = merged
            .capabilities
            .features
            .map(|f| f.memory)
            .unwrap_or_default();
        (mem, merged.capabilities.host)
    };

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

    let resolved = resolve_mcps(&config.mcp, &all_memory, &all_host, &active.tags)
        .map_err(|e| anyhow::anyhow!("failed to resolve MCP servers: {e}"))?;
    Ok(resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some(url),
        _ => None,
    }))
}

/// Build lightweight `BundleRef`s for the bundles firing in hook context.
/// All refs get precedence 1 (approximate) — sufficient for memory concat+dedup;
/// callers that need exact scalar precedence use the full `build_bundle_refs` in cli.
fn build_hook_bundle_refs(
    config_dir: &std::path::Path,
    firing: &[&crate::config::Bundle],
) -> Vec<crate::merge::BundleRef> {
    let bundles_dir = config_dir.join("bundles");
    firing
        .iter()
        .filter_map(|b| {
            let path = bundles_dir.join(&b.name);
            path.exists().then_some(crate::merge::BundleRef {
                name: b.name.clone(),
                path,
                precedence: 1,
            })
        })
        .collect()
}

/// Validate a tag to prevent query injection. Tags must be alphanumeric with
/// hyphens and underscores only (same as bundle/scope naming).
fn validate_tag(tag: &str) -> anyhow::Result<()> {
    if tag.is_empty() {
        return Err(anyhow::anyhow!("empty tag in recall query"));
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
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
    if !bundle
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow::anyhow!(
            "bundle '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            bundle
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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
        let (tool_name, content) = verbose_content(HookEvent::UserPromptSubmit, &payload);
        assert_eq!(tool_name, None);
        assert_eq!(content, "fix the bug");
    }

    #[test]
    fn verbose_content_extracts_pre_tool_use_name_and_input() {
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        });
        let (tool_name, content) = verbose_content(HookEvent::PreToolUse, &payload);
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
        let (tool_name, content) = verbose_content(HookEvent::PostToolUse, &payload);
        assert_eq!(tool_name.as_deref(), Some("Write"));
        assert!(content.contains("filePath"));
    }

    #[test]
    fn verbose_content_extracts_notification_message() {
        let payload = serde_json::json!({"message": "needs your attention"});
        let (_, content) = verbose_content(HookEvent::Notification, &payload);
        assert_eq!(content, "needs your attention");
    }

    #[test]
    fn verbose_content_extracts_stop_last_assistant_message() {
        let payload = serde_json::json!({"last_assistant_message": "done"});
        let (_, content) = verbose_content(HookEvent::Stop, &payload);
        assert_eq!(content, "done");
        let (_, content) = verbose_content(HookEvent::SubagentStop, &payload);
        assert_eq!(content, "done");
    }

    #[test]
    fn verbose_content_extracts_pre_compact_trigger() {
        let payload = serde_json::json!({"trigger": "manual", "custom_instructions": ""});
        let (_, content) = verbose_content(HookEvent::PreCompact, &payload);
        assert_eq!(content, "manual");
    }

    #[test]
    fn verbose_content_is_empty_for_missing_fields() {
        let (tool_name, content) =
            verbose_content(HookEvent::UserPromptSubmit, &serde_json::Value::Null);
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod session_log_tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx() -> ScopeContext {
        ScopeContext {
            tags: vec!["rust".into()],
            bundles: vec![],
            project: Some("llmenv".into()),
            cwd: "/tmp".into(),
            adapter: "claude-code".into(),
            llmenv_version: "3.0.0".into(),
        }
    }

    fn file_only_cfg(path: &std::path::Path) -> SessionLog {
        SessionLog {
            file: true,
            transcript: false,
            verbose: false,
            path: Some(path.to_string_lossy().into_owned()),
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
            file: false,
            transcript: false,
            ..file_only_cfg(&path)
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
            transcript: true,
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id =
            ensure_transcript_session(&cfg, Some(&client), "claude-1", &ctx(), Some(&state_path))
                .await;

        assert_eq!(id.as_deref(), Some("icm-sess-1"));
        assert_eq!(
            state::lookup_session_at(&state_path, "claude-1").as_deref(),
            Some("icm-sess-1")
        );
    }

    #[tokio::test]
    async fn ensure_transcript_session_reuses_existing_without_calling_start_session() {
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        state::record_session_at(&state_path, "claude-2", "icm-sess-2").unwrap();
        // No mock mounted: a `start_session` call here would 404 and the
        // function would have to handle/propagate that, which the assertion
        // below would catch via a mismatched id.
        let server = MockServer::start().await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let cfg = SessionLog {
            transcript: true,
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id =
            ensure_transcript_session(&cfg, Some(&client), "claude-2", &ctx(), Some(&state_path))
                .await;

        assert_eq!(id.as_deref(), Some("icm-sess-2"));
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "reusing a correlated session must not call start_session"
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
