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

use std::io::Write;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::SystemTime;

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use action::Action;
use mcp_client::McpHttpClient;
use serde_json::json;
use tracing::{debug, warn};

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
    let hook_event_name = stdin_json
        .as_ref()
        .and_then(|v| v["hook_event_name"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let claude_session_id = stdin_json
        .as_ref()
        .and_then(|v| v["session_id"].as_str().map(str::to_owned));
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
        claude_session_id.as_deref(),
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
                let out = adapter.emit_hook_context(&hook_event_name, &text);
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
/// Load config from `path`. Each hook-run is a fresh process that loads
/// config exactly once, so no cache is needed.
fn load_cached_config(path: &std::path::Path) -> anyhow::Result<crate::config::Config> {
    crate::config::Config::load(path)
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

    // #318/#864: read-once file dedup hook — computed before scope/memory
    // resolution since it doesn't need any of that. Only takes the early-
    // return fast path when session-log has no interest in PreToolUse at
    // Debug level (EventKind::ToolUse's level); otherwise falls through so
    // `run_session_log` still runs, and the read_once advisory/deny text is
    // appended to `out` further down — never unconditionally short-
    // circuiting, or enabling read_once would silently drop Debug-level
    // session logging for every PreToolUse event. Mirrors the #231 fix for
    // the task-tracker Stop hook (same early-return-drops-logging bug class).
    let read_once_text = if event == HookEvent::PreToolUse
        && let Some(ref features) = config.features
        && let Some(ref read_once) = features.read_once
        && read_once.enabled
    {
        let text = crate::hook_run::read_once::handle_pre_tool_use(
            stdin_payload,
            claude_session_id,
            read_once,
        );
        // Derived from the same `event_to_log_kind` mapping `run_session_log`
        // itself uses, rather than hardcoding `LogLevel::Debug` — a hardcoded
        // level would silently drift out of sync if `EventKind::ToolUse`'s
        // level ever changed, reintroducing this exact bug class.
        let level = event_to_log_kind(event).map_or(LogLevel::Debug, |(kind, _)| kind.log_level());
        if !log_cfg.any_sink_wants(level) {
            return Ok(text);
        }
        Some(text)
    } else {
        None
    };

    // #231: whether the task tracker's Stop reminder is wanted. Computed
    // before the #702 early-exit (below) so it can both take the cheap fast
    // path when session-log has no interest in Stop, and be appended to
    // `out` further down when session-log *does* want Stop — never
    // unconditionally short-circuiting, or enabling task_tracker would
    // silently drop Stop-event session logging (that early-return shape was
    // tried and reverted; see the git history on this block).
    let task_tracker_enabled = config
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled);
    if event == HookEvent::Stop && task_tracker_enabled && !log_cfg.any_sink_enabled() {
        let state_dir = crate::paths::state_dir()?;
        return Ok(crate::task::stop_hook_reminder(&state_dir));
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
        let mut chunk = crate::icm::generate_context_chunk(&active, &bundles);

        // Apply default type/importance markers from config (R1, R3) when no explicit
        // marker is present in the generated chunk.
        chunk = apply_memory_config_defaults(&chunk, &config, &active);

        let url = memory_url(&config, config_dir, &active)?;
        if url.is_none() {
            // Not fatal: memory actions are simply skipped below, but session
            // logging (independent of the memory backend) still proceeds.
            eprintln!("llmenv: memory {event} skipped: no memory backend active for this scope");
        }
        // Reuse MCP HTTP client across events: the memory backend URL doesn't
        // change mid-session, so the reqwest Client (connection pool, TLS state,
        // DNS cache) is only built once. Cloning the cached McpHttpClient is
        // cheap — reqwest::Client is internally Arc, and the MCP session_id is
        // shared via Arc so re-initialization is also avoided.
        static MCP_CLIENT_CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();
        let client: Option<McpHttpClient> = match url {
            Some(u) => {
                let clients = MCP_CLIENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
                let mut clients = clients.lock().unwrap_or_else(|e| e.into_inner());
                match clients.entry(u.clone()) {
                    std::collections::hash_map::Entry::Occupied(entry) => Some(entry.get().clone()),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        match McpHttpClient::new(u.clone(), HOOK_TIMEOUT) {
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
            None => None,
        };
        let state_path = Some(state::state_path());
        let ctx = build_scope_context(
            &active,
            &tags,
            &bundles,
            &env.cwd,
            adapter_name,
            claude_code_version,
        );

        // Dedup: skip Store when the context chunk hasn't changed since the last
        // SessionEnd (R3). Avoids redundant ICM writes when hooks re-run.
        if event == HookEvent::SessionEnd {
            let state_dir = crate::paths::state_dir()?;
            let dedup_path = state_dir.join(crate::paths::HOOK_STORE_CHUNK);
            let is_unchanged = std::fs::read_to_string(&dedup_path)
                .ok()
                .is_some_and(|prev| prev == chunk);
            if is_unchanged {
                debug!("chunk unchanged since last store, skipping");
                return Ok(String::new());
            }
        }

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
            if let Some(client) = &client {
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
            if event == HookEvent::SessionEnd {
                let state_dir = crate::paths::state_dir()?;
                let dedup_path = state_dir.join(crate::paths::HOOK_STORE_CHUNK);
                crate::paths::write_owner_only_atomic(&dedup_path, chunk.as_bytes())?;
            }

            // #231: append the task-tracker Stop reminder. Only reached here when
            // session-log also wants Stop (the log_cfg.any_sink_enabled() case
            // above already short-circuited before this point otherwise) — so
            // this never displaces run_session_log, it just adds to `out`.
            if event == HookEvent::Stop && task_tracker_enabled {
                let state_dir = crate::paths::state_dir()?;
                let reminder = crate::task::stop_hook_reminder(&state_dir);
                if !reminder.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&reminder);
                }
            }

            // #864: append the read_once advisory/deny text. Only reached here
            // when session-log also wants PreToolUse at Debug level (the
            // early-return above already short-circuited before this point
            // otherwise) — so this never displaces run_session_log, it just adds
            // to `out`.
            if let Some(text) = &read_once_text
                && !text.is_empty()
            {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
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
            // #867: an already-computed read_once result must not be lost to
            // an unrelated pipeline error — recover it instead of letting `?`
            // propagate the error past the point where it was decided.
            if let Some(text) = read_once_text {
                warn!(
                    error = %e,
                    "hook-run: pipeline failed after read_once already computed a \
                     result; returning it instead of silently dropping it"
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
            ensure_transcript_session(call.log_cfg, call.client, csid, call.ctx, call.state_path)
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
    tags: &[String],
    bundles: &[String],
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
        tags: tags.to_vec(),
        bundles: bundles.to_vec(),
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
            ensure_transcript_session(cfg, client, csid, ctx, state_path).await
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

/// Find the resolved memory backend's HTTP URL for the active tags, if any.
///
/// Cached result of a bundle merge: the memory entries and host map extracted
/// from `MergedManifest`. Keyed by config + bundle identity so the full merge
/// (disk I/O + YAML parse + tree walk) is skipped on repeat calls.
struct MergeCacheEntry {
    key: u64,
    bundle_memory: Vec<crate::config::Memory>,
    bundle_host: std::collections::BTreeMap<String, crate::config::HostEntry>,
}

/// Compute a cache key for the merge result: config mtime (detects config.yaml
/// edits) + sorted firing bundle names. Does not hash bundle file contents —
/// those don't change mid-session.
fn merge_cache_key(firing: &[&crate::config::Bundle]) -> anyhow::Result<u64> {
    let config_path = crate::paths::config_path()?;
    let mtime = config_path
        .metadata()
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut hasher = DefaultHasher::new();
    mtime.hash(&mut hasher);
    for b in firing {
        b.name.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

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
    // ponytail: merge cache keyed on config mtime + firing bundle names. Does not
    // detect bundle file content edits (AGENTS.md, subdir files) — acceptable
    // since they never change mid-session. Config mtime covers config.yaml edits.
    static MERGE_CACHE: Mutex<Option<MergeCacheEntry>> = Mutex::new(None);
    let (bundle_memory, bundle_host) = if bundle_refs.is_empty() {
        (Vec::new(), std::collections::BTreeMap::new())
    } else {
        let cache_key = merge_cache_key(&firing)?;
        let mut cache = MERGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.as_ref()
            && entry.key == cache_key
        {
            (entry.bundle_memory.clone(), entry.bundle_host.clone())
        } else {
            let merged = crate::merge::merge(&config.capabilities, &config.native, &bundle_refs)
                .unwrap_or_default();
            let mem = merged
                .capabilities
                .features
                .map(|f| f.memory)
                .unwrap_or_default();
            let host = merged.capabilities.host;
            *cache = Some(MergeCacheEntry {
                key: cache_key,
                bundle_memory: mem.clone(),
                bundle_host: host.clone(),
            });
            (mem, host)
        }
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
    chunk: &str,
    config: &crate::config::Config,
    active: &crate::scope::ActiveScopes,
) -> String {
    let Some(mem) = config.features.as_ref().and_then(|f| {
        f.memory
            .iter()
            .find(|m| m.when.iter().any(|t| active.tags.contains(t)))
    }) else {
        return chunk.to_string();
    };

    let mut out = chunk.to_string();

    if !chunk.contains("<!-- llmenv-type:")
        && let Some(ty) = &mem.default_type
    {
        out.push_str(&format!("\n<!-- llmenv-type: {} -->", ty.as_marker_str()));
    }

    if !chunk.contains("<!-- llmenv-importance:")
        && let Some(imp) = &mem.default_importance
    {
        out.push_str(&format!(
            "\n<!-- llmenv-importance: {} -->",
            imp.as_marker_str()
        ));
    }

    out
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
/// level and never propagated to the caller.
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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

/// Spawn a detached child to run post-session consolidation. Best-effort
/// fire-and-forget — spawn failures are logged at debug level and the caller
/// never waits on the child.
fn post_session_consolidation() {
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("consolidation-run: cannot resolve current_exe");
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("consolidation-run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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
        }
    }

    #[test]
    fn apply_memory_defaults_idempotent_no_type() {
        let config = memory_config(None);
        let active = active_with_tag("test");
        let input = "## context\nno markers";
        let once = apply_memory_config_defaults(input, &config, &active);
        let twice = apply_memory_config_defaults(&once, &config, &active);
        assert_eq!(once, twice, "applying defaults twice must be idempotent");
    }

    #[test]
    fn apply_memory_defaults_adds_type_marker_when_present() {
        let config = memory_config(Some(llmenv_config::MemoryType::Semantic));
        let active = active_with_tag("test");
        let input = "## context";
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
        let input = "## context\n<!-- llmenv-type: episodic -->";
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
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                retention_days: None,
            }),
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
