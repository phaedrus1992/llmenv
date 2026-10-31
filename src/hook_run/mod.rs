//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

pub(crate) mod action;
mod adaptive;
pub(crate) mod agent_config;
mod bundle_names;
pub(crate) mod cbm_index_guard;
pub(crate) mod cd_guard;
pub(crate) mod checkpoint;
pub(crate) mod detached_cbm;
pub(crate) mod detached_consolidation;
pub(crate) mod detached_store;
mod engine_rules;
mod event;
mod memory_endpoint;
use memory_endpoint::resolve_memory_client;
mod notice;
use bundle_names::recall_bundle_names;
use engine_rules::blocks_by_exit_code;
#[cfg(test)]
pub(crate) use event::HOOK_EVENT_NAMES;
pub(crate) use event::HookEvent;
use event::engine_event_for;
use notice::{
    log_memory_without_channel, log_notice_without_channel, write_hook_line, write_notice_only,
};
use transcript_link::ensure_transcript_session;
mod cbm_index_cmd;
mod consolidation_spawn;
pub(crate) mod features_scope;
pub(crate) mod idempotency;
pub(crate) mod mcp_client;
pub(crate) mod mcp_health;
pub(crate) mod read_once;
#[cfg(test)]
use consolidation_spawn::post_session_consolidation_in;
use consolidation_spawn::{consolidation_run_command, post_session_consolidation};
mod recall;
pub(crate) mod recall_health;
mod record_hash;
mod relevance;
pub(crate) mod repeat_detect;
mod respawn;
mod resume_cache;
mod session_emit;
mod session_ledger;
mod session_notices;
pub(crate) mod session_state;
mod shell_split;
pub(crate) mod slippage;
mod slippage_read;
pub(crate) mod stop_dedupe;
mod timing;
pub(crate) use cbm_index_cmd::index_result_path;
use cbm_index_cmd::{
    create_result_file, direct_index_stdout, index_command, index_job_command, uses_wrapper,
};
mod stop_reminder;
pub(crate) mod task_nudge;
pub(crate) mod task_tools;
pub(crate) mod transcript;
mod transcript_link;
use respawn::respawn_job;
use session_emit::emit_session_log;
use stop_reminder::resolve_stop_reminder;
mod global_topics;
mod web_fetch;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod agent_state_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod cbm_trigger_tests;

use std::io::Write;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use std::collections::{BTreeMap, HashMap};

use action::Action;
use anyhow::Context as _;
use mcp_client::McpHttpClient;
use tracing::{debug, error, warn};

use crate::config::SessionLog;
use crate::hook_run::action::WakeUpArgs;
use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::mcp::resolve::{MemoryHookSettings, ResolvedKind, resolve_mcps};
use crate::session_log::event::{EventKind, EventScope, SessionLogEvent, now_rfc3339};
use crate::session_log::{ScopeContext, scope_header_content, scope_metadata_json, state};
use llmenv_config::LogLevel;
use web_fetch::{handle_web_fetch_post_tool_use, icm_store_command};

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
                error!(tag = %tag, error = %e, "tag name validation failed");
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

/// The ordered actions to run for an event, given the active tags' and bundles'
/// recall queries (built by [`tag_recall_queries`] and [`bundle_recall_queries`],
/// the single sources of tag→recall and bundle→recall expansion), plus the
/// wake-up arguments (the configured token budget, #1216, and the session's
/// project, #2249), threaded straight into `Action::WakeUp` on `SessionStart`.
///
/// `TurnStart` runs the most specific recalls first (#2159): every rank-1 tag (project scope
/// and `$LLMENV_EXTRA_TAGS`), then every active bundle, then the tags of the broader scopes in
/// rank order (see [`recall::tag_specificity`]; a tag with no entry in `ranks` is unscoped), and
/// last the natural-language `Recall`, because it has no project filter and is the least
/// specific. Tag and bundle recalls are project-unfiltered (#197, #228). The turn-capture
/// events carry no memory actions.
///
/// `wake` is `None` for a continued session (see [`continues_session`]), which gets no
/// wake-up call.
fn dispatch(
    event: HookEvent,
    tag_queries: &[TagRecallQuery],
    bundle_queries: &[BundleRecallQuery],
    ranks: &BTreeMap<String, u8>,
    wake: Option<&WakeUpArgs>,
) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => wake.cloned().map(Action::WakeUp).into_iter().collect(),
        HookEvent::TurnStart => {
            let mut actions = scope_recall_actions(tag_queries, bundle_queries, ranks);
            let project = wake.and_then(|w| w.project.clone()).unwrap_or_default();
            actions.push(Action::Recall(project));
            actions
        }
        HookEvent::SessionEnd => vec![Action::Store],
        HookEvent::UserPromptSubmit
        | HookEvent::PreToolUse
        | HookEvent::PostToolUse
        | HookEvent::Notification
        | HookEvent::Stop
        | HookEvent::SubagentStop
        | HookEvent::PreCompact
        | HookEvent::PostToolBatch
        | HookEvent::PostToolUseFailure
        | HookEvent::SubagentStart
        | HookEvent::SubagentTask
        | HookEvent::PostModelSwitch => vec![],
        HookEvent::PostSession => vec![], // consolidation runs as a separate step
    }
}

/// Whether a `SessionStart` payload continues an earlier conversation (#2142).
/// Claude Code's `source` is `resume` or `fork` then; the conversation already
/// holds the earlier wake-up pack, so a second one only doubles the context.
fn continues_session(payload: &serde_json::Value) -> bool {
    matches!(payload["source"].as_str(), Some("resume" | "fork"))
}

/// The scope-tag recalls in specificity order (#2159): rank-1 tags, bundles, then
/// broader tags.
fn scope_recall_actions(
    tag_queries: &[TagRecallQuery],
    bundle_queries: &[BundleRecallQuery],
    ranks: &BTreeMap<String, u8>,
) -> Vec<Action> {
    let rank_of = |q: &TagRecallQuery| ranks.get(&q.tag).copied().unwrap_or(recall::UNSCOPED_RANK);
    let mut tags: Vec<&TagRecallQuery> = tag_queries.iter().collect();
    // Stable, so tags of one rank keep the caller's (alphabetical) order.
    tags.sort_by_key(|q| rank_of(q));
    let split = tags.partition_point(|q| rank_of(q) <= recall::MOST_SPECIFIC_RANK);
    let (specific, broader) = tags.split_at(split);
    let mut actions: Vec<Action> = Vec::new();
    actions.extend(specific.iter().map(|q| Action::RecallTag((*q).clone())));
    actions.extend(bundle_queries.iter().cloned().map(Action::RecallBundle));
    actions.extend(broader.iter().map(|q| Action::RecallTag((*q).clone())));
    actions
}

/// Whether the previously-stored dedup snapshot (R3) matches the chunk about
/// to be stored. `prev` is `None` when no snapshot exists yet (first
/// SessionEnd, or a read failure) — always "changed" in that case. Extracted
/// as a pure function so the comparison is directly unit-testable without
/// standing up `run_inner`'s full config/MCP harness. (#1792)
fn chunk_unchanged(prev: Option<&str>, current: &str) -> bool {
    prev.is_some_and(|p| p == current)
}

/// Which chunk `run_inner` stores for a given event: the minimal
/// (boilerplate-free) `storage_chunk` for `SessionEnd`, the full `chunk`
/// (used for hook-context injection) otherwise. Extracted as a pure function
/// so the SessionEnd-uses-the-minimal-chunk invariant is directly
/// unit-testable. (#1792)
fn store_content_for_event<'a>(
    event: HookEvent,
    chunk: &'a str,
    storage_chunk: &'a str,
) -> &'a str {
    match event {
        HookEvent::SessionEnd => storage_chunk,
        _ => chunk,
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
        HookEvent::PostToolBatch
        | HookEvent::PostToolUseFailure
        | HookEvent::SubagentStart
        | HookEvent::SubagentTask
        | HookEvent::PostModelSwitch => None,
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
        | HookEvent::PostSession
        | HookEvent::PostToolBatch
        | HookEvent::PostToolUseFailure
        | HookEvent::SubagentStart
        | HookEvent::SubagentTask
        | HookEvent::PostModelSwitch => (None, String::new()),
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

/// The session's working directory: the hook payload's `cwd`, else the process cwd.
fn hook_cwd(payload: &serde_json::Value, fallback: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(payload["cwd"].as_str().unwrap_or(fallback))
}

/// The `icm_wake_up` arguments for `event`, whose project also scopes the
/// stateless `TurnStart` recall (#2253). The project lookup runs git, so only
/// `SessionStart` and a stateless `TurnStart` pay for it; the adaptive
/// `TurnStart` reads the project from its session ledger. Whether a `TurnStart`
/// is stateless follows [`uses_adaptive`], so a missing session id still scopes.
///
/// # Errors
/// The project cannot be named. The caller skips the event's memory work, because a recall
/// without a project scope would return other projects' memories (#2688).
fn wake_args(
    event: HookEvent,
    settings: Option<&MemoryHookSettings>,
    payload: &serde_json::Value,
    cwd: &str,
    session_id: Option<&str>,
) -> anyhow::Result<WakeUpArgs> {
    // The adaptive flow also needs a state dir. Without one, the stateless actions run, and
    // they recall with the project this lookup supplies (#2765).
    let adaptive_turn =
        uses_adaptive(event, settings, session_id) && crate::paths::state_dir().is_ok();
    let wants_project = match event {
        HookEvent::SessionStart => true,
        HookEvent::TurnStart => !adaptive_turn,
        _ => false,
    };
    let project = if wants_project {
        Some(crate::memory::project::session_project(&hook_cwd(
            payload, cwd,
        ))?)
    } else {
        None
    };
    Ok(WakeUpArgs {
        max_tokens: settings.and_then(|s| s.wakeup_max_tokens),
        project,
    })
}

/// Whether `event` writes to the recall ledger without any MCP call (#2249).
fn records_locally(event: HookEvent) -> bool {
    matches!(event, HookEvent::PostToolBatch | HookEvent::SubagentTask)
}

/// Whether `event` takes the adaptive recall flow instead of the stateless actions.
/// An unsafe `session_id` cannot key a ledger file, so it takes the stateless actions.
fn uses_adaptive(
    event: HookEvent,
    settings: Option<&MemoryHookSettings>,
    session_id: Option<&str>,
) -> bool {
    settings.is_some_and(|s| s.adaptive_recall)
        && session_id.is_some_and(crate::paths::is_valid_short_name)
        && matches!(
            event,
            HookEvent::SessionStart
                | HookEvent::TurnStart
                | HookEvent::PostToolUseFailure
                | HookEvent::SubagentStart
        )
}

/// The inputs of [`run_event_memory`], grouped to stay under the positional
/// parameter limit.
struct MemoryCall<'a> {
    event: HookEvent,
    client: &'a McpHttpClient,
    settings: Option<&'a MemoryHookSettings>,
    session_id: Option<&'a str>,
    state_dir: Option<std::path::PathBuf>,
    payload: &'a serde_json::Value,
    actions: Vec<Action>,
    scope: Vec<Action>,
    wake: WakeUpArgs,
    query: &'a str,
    store_content: &'a str,
}

/// Run the event's memory work: the adaptive flow when it applies, else the
/// stateless actions. A missing state dir degrades to the stateless actions.
async fn run_event_memory(call: MemoryCall<'_>) -> anyhow::Result<HookText> {
    let Some(session_id) = call.session_id else {
        return Ok(
            run_memory_actions(call.client, call.actions, call.query, call.store_content).await,
        );
    };
    // A separate bind keeps the session id out of the state dir path's data flow,
    // which CodeQL tracks through a tuple (rust/cleartext-logging).
    let Some(state_dir) = call.state_dir.as_deref() else {
        return Ok(
            run_memory_actions(call.client, call.actions, call.query, call.store_content).await,
        );
    };
    if !uses_adaptive(call.event, call.settings, Some(session_id)) {
        return Ok(
            run_memory_actions(call.client, call.actions, call.query, call.store_content).await,
        );
    }
    let store = session_ledger::LedgerStore::new(state_dir);
    let ctx = adaptive::AdaptiveCtx {
        client: call.client,
        store: &store,
        session_id,
        payload: call.payload,
        global_topics: call.settings.map_or(&[], |s| &s.global_topics),
        state_dir,
    };
    match call.event {
        HookEvent::SessionStart => {
            adaptive::session_start(&ctx, Action::WakeUp(call.wake), call.scope).await
        }
        HookEvent::TurnStart => adaptive::turn_start(&ctx, call.scope).await,
        HookEvent::PostToolUseFailure => adaptive::tool_failure(&ctx).await,
        HookEvent::SubagentStart => adaptive::subagent_start(&ctx).await,
        _ => {
            Ok(run_memory_actions(call.client, call.actions, call.query, call.store_content).await)
        }
    }
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
/// Whether this event empties the model's context, so the per-session "already
/// seen" state must reset (#2381).
fn resets_read_state(event: HookEvent, payload: &serde_json::Value) -> bool {
    event == HookEvent::SessionStart && session_state::context_was_lost(payload["source"].as_str())
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

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(HookExit::Success)` even when the backend is unreachable — only an
/// explicit deny asks the caller for a non-zero exit.
pub(crate) fn run(event: &str, engine: &str) -> anyhow::Result<HookExit> {
    use std::io::Read;

    let mut stdin_buf = String::new();
    // Without the input the hook has no session id, so every per-session feature turns off.
    // The agent is told, because a log line or stderr never reaches it.
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        crate::agent_notice::record(format!(
            "llmenv hook-run could not read its input, so per-session features are off: {e}. \
             Fix: check how the engine runs the hook command."
        ));
    }
    let mut stdin_json = serde_json::from_str::<serde_json::Value>(&stdin_buf)
        .inspect_err(|e| {
            // Empty stdin is no payload, not a broken one, so it adds no notice.
            if !stdin_buf.trim().is_empty() {
                crate::agent_notice::record(format!(
                    "llmenv hook-run could not parse its input, so per-session features are off: \
                     {e}. Fix: check that the engine sends JSON on stdin."
                ));
            }
        })
        .ok();
    engine_rules::neutralize_for_engine(engine, stdin_json.as_mut());
    // Unparseable stdin leaves the engine's name unknown, so the argv event supplies it (#2748).
    let hook_event_name = engine_event_for(
        stdin_json
            .as_ref()
            .and_then(|v| v["hook_event_name"].as_str()),
        event,
    );
    let claude_session_id: Option<&str> =
        stdin_json.as_ref().and_then(|v| v["session_id"].as_str());
    // The version is informational. Failing the hook here would exit 1, which lets the tool call
    // run unchecked, so a value that is not UTF-8 is logged and read as unset.
    let claude_code_version = match crate::env_var::utf8_var("CLAUDE_CODE_VERSION") {
        Ok(version) => version.unwrap_or_default(),
        Err(e) => {
            tracing::error!("{e:#}");
            String::new()
        }
    };

    let adapter = crate::adapter::adapter_for_engine(engine);
    let parsed = match HookEvent::from_str(event) {
        Ok(e) => e,
        Err(e) => {
            // stderr from a hook is dropped by the engine, so the agent gets the notice on stdout (#2748).
            crate::agent_notice::record(format!(
                "llmenv hook-run: {e}. Fix: check the hook command's event name."
            ));
            if let Some(notice) = crate::agent_notice::current() {
                write_notice_only(adapter.as_ref(), &hook_event_name, event, &notice);
            }
            return Ok(HookExit::Success);
        }
    };
    let null_payload = serde_json::Value::Null;
    let payload = stdin_json.as_ref().unwrap_or(&null_payload);

    // #741: the drift check runs from here rather than from its own
    // `SessionStart` hook. Both were registered unconditionally on the same
    // event, so a session start spawned two `llmenv` processes that each parsed
    // the config; folding it in leaves one, and leaves one place where "does
    // session start check for drift" is decided.
    //
    // Fail-soft on purpose — a hook that can't answer "has the config drifted"
    // must not take down memory wake-up or session logging with it.
    timing::check_stale_with_timing(parsed, adapter.name());

    let result = run_inner(
        parsed,
        claude_session_id,
        payload,
        adapter.name(),
        &claude_code_version,
    );
    // A failed memory step must not read as "nothing to recall" to the agent.
    if let Err(e) = &result {
        crate::agent_notice::record(memory_skipped_notice(event, e));
    }
    // Read after `run_inner`, so every output path below carries it to the agent (#2658).
    let notice = crate::agent_notice::current();
    match result {
        Ok(text) => {
            // #318: deny envelope detected — write a proper deny JSON envelope
            // to stdout so the Claude Code engine blocks the tool call.
            if text.notes.starts_with("__DENY__:") {
                if deny_drops_memory(&text) {
                    // The deny envelope replaces the whole output, so this memory is not sent.
                    tracing::error!("hook-run: memory text dropped for a deny envelope");
                }
                let reason = text.notes.trim_start_matches("__DENY__:");
                let reason = match notice {
                    Some(notice) => format!("{notice}\n\n{reason}"),
                    None => reason.to_string(),
                };
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
                    // The opencode plugin blocks on exit 2 and shows stderr as the
                    // reason; it also reads the envelope, but exit 2 is the one signal
                    // that every hook bridge agrees on. Claude Code is left on exit 0
                    // deliberately: it already blocks on the envelope alone,
                    // and exiting 2 there would change a working path for no
                    // gain.
                    eprintln!("{reason}");
                    return Ok(HookExit::Block);
                }
            } else {
                let notes = join_notes(notice.as_deref(), &text.notes);
                let out = adapter.emit_hook_context(&hook_event_name, &text.memory, &notes);
                let out = resume_cache::with_resume_notice(out, &hook_event_name, payload);
                // The omission notice rides in `notes`, so the log must see the joined text (#2764).
                log_notice_without_channel(&out, Some(notes.as_str()), event);
                log_memory_without_channel(&out, &text.memory, event);
                if let Some(e) = write_hook_line(&mut std::io::stdout(), &out) {
                    eprintln!("llmenv: failed to write hook output: {e}");
                }
            }
        }
        Err(e) => {
            eprintln!("llmenv: memory {event} skipped: {e}");
            // stderr from a hook is dropped by the engine, so the notice goes on stdout (#2658).
            if let Some(notice) = notice {
                write_notice_only(adapter.as_ref(), &hook_event_name, event, &notice);
            }
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
/// The state folder, or `None` after an error that names what the missing folder disables.
/// The error level shows at the default log level, so a hook that cannot keep its state is seen.
fn state_dir_or_log(disabled: &str) -> Option<std::path::PathBuf> {
    crate::paths::state_dir()
        .inspect_err(|e| {
            crate::agent_notice::record(format!(
                "llmenv has no state directory, so {disabled}: {e:#}. \
                 Fix: run `llmenv doctor` to check the state directory."
            ));
        })
        .ok()
}

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
    let roots_deny = crate::hook_run::cbm_index_guard::handle_roots(stdin_payload, config);
    if !roots_deny.is_empty() {
        return Some(roots_deny);
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
            // The commit gate reads its state from the same directory, so it denies rather than
            // passing a commit it cannot check (#2630).
            if task_tracker_enabled
                && let Some(tracker) = config
                    .features
                    .as_ref()
                    .and_then(|f| f.task_tracker.as_ref())
                && let Some(deny) =
                    crate::hook_run::task_nudge::state_dir_deny(tracker, stdin_payload, &e)
            {
                return Some(deny);
            }
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
    // Each subagent keeps its own hook state (#2936). Repeat detection derives the same key.
    let agent_key = claude_session_id.map(|id| session_state::agent_state_key(id, stdin_payload));
    let state_id = agent_key.as_deref();
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
        state_id,
        state_dir,
    );
    if !write_guard.is_empty() {
        return Some(write_guard);
    }

    // #2456: the first commit or pull request with no task in progress is denied once.
    if task_tracker_enabled
        && let Some(tracker) = config
            .features
            .as_ref()
            .and_then(|f| f.task_tracker.as_ref())
    {
        let deny = crate::hook_run::task_nudge::handle_pre_tool_use(
            tracker,
            stdin_payload,
            state_id,
            state_dir,
        );
        if !deny.is_empty() {
            return Some(deny);
        }
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
            state_id,
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

/// Hook output, split by source. Only `memory` came from ICM, so only it gets the ICM header.
/// `notes` is llmenv text: the reads, nudges, reminders, and any deny envelope (#2754).
#[derive(Debug, Default, PartialEq, Eq)]
struct HookText {
    memory: String,
    notes: String,
}

impl HookText {
    /// Output that holds no ICM memory.
    fn from_notes(notes: String) -> Self {
        Self {
            memory: String::new(),
            notes,
        }
    }

    /// The memory and the notes as one block, for assertions on the whole output.
    #[cfg(test)]
    fn text(&self) -> String {
        [self.memory.as_str(), self.notes.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Whether a deny envelope would replace memory text that was computed in the same run.
fn deny_drops_memory(text: &HookText) -> bool {
    !text.memory.is_empty()
}

/// Puts the agent notice in front of the llmenv notes, with no empty separator.
fn join_notes(notice: Option<&str>, notes: &str) -> String {
    [notice.unwrap_or_default(), notes]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn run_inner(
    event: HookEvent,
    claude_session_id: Option<&str>,
    stdin_payload: &serde_json::Value,
    adapter_name: &str,
    claude_code_version: &str,
) -> anyhow::Result<HookText> {
    let t0 = std::time::Instant::now();
    let config_path = crate::paths::config_path()?;
    let config = load_cached_config(&config_path)?;
    // #2460: a feature that only a bundle sets must read as set here, as it does in the adapter.
    let config = features_scope::resolve(config, &config_path);
    // Resolved before any notice is built. A later `?` would drop the notices (#2719).
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
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
    // #2249: local ledger writes need no scope or MCP work, so they run before every
    // early return in this function.
    if records_locally(event)
        && let (Some(session_id), Ok(state_dir)) = (
            claude_session_id,
            crate::paths::state_dir()
                .inspect_err(|e| tracing::error!("no state dir, recall ledger skipped: {e}")),
        )
    {
        adaptive::record_local(
            event,
            &session_ledger::LedgerStore::new(&state_dir),
            session_id,
            stdin_payload,
        );
    }

    // #2381: a compaction empties the model's context, so the per-session "already read" state
    // must go before the first Read or Edit of the new context.
    if resets_read_state(event, stdin_payload)
        && let Some(session_id) = claude_session_id
    {
        match crate::paths::state_dir() {
            Ok(state_dir) => session_state::reset_read_state(&state_dir, session_id),
            Err(e) => tracing::error!("no state dir, read state not reset after compaction: {e}"),
        }
    }

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
                    crate::trace::emit_trace_timing(crate::trace::HookTiming::new(t0, t_config));
                    return Ok(HookText::from_notes(t));
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
    let agent_key = claude_session_id.map(|id| session_state::agent_state_key(id, stdin_payload));
    if counts_tool_use(event)
        && let Some(state_dir) = state_dir_or_log("slippage counters not updated")
    {
        crate::hook_run::slippage::handle_post_tool_use(
            config.features.as_ref().and_then(|f| f.slippage.as_ref()),
            stdin_payload,
            agent_key.as_deref(),
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
                crate::trace::emit_trace_timing(crate::trace::HookTiming::new(t0, t_config));
                return Ok(HookText::from_notes(text));
            }
            Some(text)
        }
    } else {
        None
    };

    // #2511: a new user prompt makes an unchanged Stop reminder due again.
    if event == HookEvent::UserPromptSubmit
        && task_tracker_enabled
        && let Some(state_dir) = state_dir_or_log("stop reminder guard not re-armed")
    {
        stop_dedupe::forget(&state_dir, claude_session_id);
    }

    // #2456: the nudges after a tool call. They need no scope or memory work. They are not
    // returned early, because a PostToolUse also feeds the WebFetch auto-store below.
    let nudge_text = if event == HookEvent::PostToolUse
        && task_tracker_enabled
        && let Some(tracker) = config
            .features
            .as_ref()
            .and_then(|f| f.task_tracker.as_ref())
        && let Some(state_dir) = state_dir_or_log("task nudges skipped")
    {
        let text = crate::hook_run::task_nudge::handle_post_tool_use(
            tracker,
            stdin_payload,
            agent_key.as_deref(),
            &state_dir,
        );
        (!text.is_empty()).then_some(text)
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
        // A missing state dir skips the reminder and logs why. A `?` here would drop the Stop
        // output with it (#2719).
        let reminder =
            state_dir_or_log("stop reminder skipped").map_or_else(String::new, |state_dir| {
                resolve_stop_reminder(&state_dir, claude_session_id, &config, stdin_payload)
            });
        crate::trace::emit_trace_timing(crate::trace::HookTiming::new(t0, t_config));
        return Ok(HookText::from_notes(reminder));
    }

    // #2398: a model switch only updates the session's agent-config document. It needs no scope
    // and no memory call, so it returns before the pipeline below.
    if event == HookEvent::PostModelSwitch {
        match (claude_session_id, crate::paths::state_dir()) {
            (Some(session_id), Ok(state_dir)) => agent_config::record_model_switch(
                &state_dir,
                session_id,
                stdin_payload,
                adapter_name,
                session_state::unix_now(),
            ),
            (None, _) => tracing::debug!("post_model_switch has no session id, skipped"),
            (_, Err(e)) => tracing::error!("no state dir, model switch not recorded: {e}"),
        }
        crate::trace::emit_trace_timing(crate::trace::HookTiming::new(t0, t_config));
        return Ok(HookText::default());
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
    let pipeline_result: anyhow::Result<HookText> = (|| {
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
                | HookEvent::PostToolUseFailure
                | HookEvent::SubagentStart
        ) && !log_cfg.any_sink_enabled()
        {
            crate::trace::emit_trace_timing(crate::trace::HookTiming::new(t0, t_config));
            return Ok(HookText::default());
        }

        let env = crate::scope::matcher::Env::detect_for_config(&config)?;
        let active = crate::scope::evaluate(&config, &env);
        let t_scope = std::time::Instant::now();

        // Recall queries are validated before the SessionStart notices are built. A `?` after
        // them would return from the hook and drop the notices (#2709). A bad tag therefore
        // fails the hook before the codebase-memory block below runs, which is the same
        // failure the hook already reported, one step earlier.
        let tags = active.tags.iter().cloned().collect::<Vec<_>>();
        let bundles = recall_bundle_names(&config.bundle, &active);
        // Build per-tag and per-bundle recall queries. Validation rejects query
        // injection; these are the single sources of the tag/bundle→keyword encoding.
        let tag_queries = tag_recall_queries(&tags)?;
        let bundle_queries = bundle_recall_queries(&bundles)?;

        // #365: register the active project with codebase-memory-mcp's own
        // background auto-watch on SessionStart. Deliberately NOT gated on an
        // ICM memory client being configured (unlike PostSession/PostToolUse
        // below) — read_once/codebase_memory are fully orthogonal to ICM.
        // Fire-and-forget: indexing a large repo can take minutes (the Linux
        // kernel benchmarks at ~3 per upstream docs), so this must never
        // block SessionStart.
        // Per-hook-event timings: captured in this block, read by emit_trace_timing inside the async closure.
        let mut checkpoint_us: Option<u64> = None;
        let mut session_log_us: Option<u64> = None;
        let mut roots_notice: Option<String> = None;
        let mut cbm_roots_took: Option<std::time::Duration> = None;
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
            checkpoint_us = match active_codebase_memory.as_slice() {
                [] => None,
                [cm] => match crate::mcp::resolve::codebase_memory_paths() {
                    Ok((project_root, state_dir)) => {
                        // Before the index run, so the server accepts the repository (#2406).
                        let t_roots = std::time::Instant::now();
                        let roots = crate::mcp::cbm_roots::session_start_notice(cm, &project_root);
                        cbm_roots_took = Some(t_roots.elapsed());
                        let t_checkpoint = std::time::Instant::now();
                        let index = trigger_codebase_memory_index(&project_root, cm, &state_dir);
                        roots_notice = match (roots, index) {
                            (Some(r), Some(i)) => Some(format!("{r}{i}")),
                            (r, i) => r.or(i),
                        };
                        Some(std::time::Duration::as_micros(&t_checkpoint.elapsed()) as u64)
                    }
                    Err(e) => {
                        tracing::error!(
                            "cannot resolve the codebase-memory paths, so the session-start index \
                             is skipped: {e:#}"
                        );
                        roots_notice = Some(index_skipped_notice(&format!(
                            "cannot resolve the project paths: {e:#}"
                        )));
                        None
                    }
                },
                _ => {
                    tracing::debug!(
                        "codebase_memory: multiple entries active simultaneously, \
                         not triggering SessionStart index"
                    );
                    None
                }
            };
        }

        // Recall query: the sorted active tags. Store content: the llmenv context
        // chunk (tags/bundles/project). `active.tags` is a BTreeSet, so this
        // iteration order is already sorted ascending — no separate sort needed.
        let tag_ranks = recall::tag_specificity(&active);
        let query = tags.join(", ");
        // Generate full chunk for injection into hook context, and minimal chunk
        // for storage. The minimal chunk omits boilerplate instruction text
        // that is provided once in the SessionStart hook injection, not stored
        // per-project. This reduces per-turn token waste. (#1792)
        let mut chunk = crate::icm::generate_context_chunk(&active, &bundles);
        let mut storage_chunk = crate::icm::generate_minimal_chunk(&active, &bundles);

        // #317: folded into the chunk the SessionEnd store already sends,
        // rather than issuing a second `icm_memory_store` — one store per
        // session end keeps the memory readable and halves the round trips.
        // Append session metrics to both chunks (injection and storage).
        if stores_session_metrics(event)
            && let Some(state_dir) = state_dir_or_log("session metrics not stored")
            && let Some(summary) = crate::hook_run::slippage::session_metrics_summary(
                config.features.as_ref().and_then(|f| f.slippage.as_ref()),
                claude_session_id,
                &state_dir,
            )
        {
            chunk.push_str("\n\n");
            chunk.push_str(&summary);
            storage_chunk.push_str("\n\n");
            storage_chunk.push_str(&summary);
        }

        // Reuse MCP HTTP client across events: the memory backend URL doesn't
        // change mid-session, so the reqwest Client (connection pool, TLS state,
        // DNS cache) is only built once. Cloning the cached McpHttpClient is
        // cheap — reqwest::Client is internally Arc, and the MCP session_id is
        // shared via Arc so re-initialization is also avoided.
        static MCP_CLIENT_CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();
        let resolved_client =
            resolve_memory_client(&config, config_dir, &active, event, &MCP_CLIENT_CACHE);
        let settings = resolved_client.as_ref().map(|r| r.settings.clone());
        // Default type/importance markers (R1, R3) from the resolved entry, which
        // can be top-level or bundle-declared, when the chunk has none of its own.
        chunk = apply_memory_config_defaults(chunk, settings.as_ref());
        storage_chunk = apply_memory_config_defaults(storage_chunk, settings.as_ref());
        // A failed project lookup skips this event's memory work, and the agent is told why (#2688).
        // With no client, no memory call runs, so the lookup (a git subprocess) would go unused.
        // The dispatch below needs a client too, so a missing project never reaches it.
        let wake_result = resolved_client.as_ref().map(|_| {
            wake_args(
                event,
                settings.as_ref(),
                stdin_payload,
                &env.cwd,
                claude_session_id,
            )
        });
        let (wake, memory_notice) = match wake_result {
            None => (WakeUpArgs::default(), None),
            Some(Ok(args)) => (args, None),
            Some(Err(e)) => {
                tracing::error!("memory skipped for {event}: {e:#}");
                let args = WakeUpArgs {
                    max_tokens: settings.as_ref().and_then(|s| s.wakeup_max_tokens),
                    project: None,
                };
                (args, Some(format!("llmenv: memory {event} skipped: {e:#}")))
            }
        };
        let memory_skipped = memory_notice.is_some();
        let client = resolved_client.map(|r| r.client);
        let state_path = state::state_path()
            .inspect_err(|e| {
                error!(error = %e, "no ICM transcript state: the session map is unavailable");
                crate::agent_notice::record(format!(
                    "llmenv could not open the ICM transcript state, so this session's transcript link is \
                     not recorded: {e:#}. Fix: run `llmenv doctor` to check the state directory."
                ));
            })
            .ok();
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
        // A missing state directory skips the dedup snapshot. It does not end the hook, which
        // would drop the notices built above (#2719).
        let session_end_dedup_path = dedup_snapshot_path(event, crate::paths::state_dir);
        let session_end_unchanged = if let Some(dedup_path) = &session_end_dedup_path {
            let prev = read_dedup_snapshot(dedup_path);
            let is_unchanged = chunk_unchanged(prev.as_deref(), &storage_chunk);
            if is_unchanged {
                debug!("chunk unchanged since last store, skipping");
                if !log_cfg.any_sink_enabled() {
                    // An unchanged chunk skips the store, not the consolidation.
                    if client.is_some() {
                        maybe_start_consolidation(
                            event,
                            &config,
                            config_dir,
                            &active,
                            post_session_consolidation,
                        );
                    }
                    crate::trace::emit_trace_timing(
                        crate::trace::HookTiming::new(t0, t_config).through_scope(t_scope),
                    );
                    return Ok(HookText::default());
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
        // Without a runtime only the runtime-bound work is skipped. The notices and the
        // nudge and pre-tool texts built above still reach the agent (#2719).
        let (rt, runtime_failure) = match rt {
            Ok(rt) => (Some(rt), None),
            Err(e) => {
                let failure =
                    format!("llmenv: {event} memory skipped, the tokio runtime cannot start: {e}");
                tracing::error!("{failure}");
                (None, Some(failure))
            }
        };
        let session_log = SessionLogCall {
            log_cfg: &log_cfg,
            client: client.as_ref(),
            claude_session_id,
            ctx: &ctx,
            state_path: state_path.as_deref(),
        };
        let agent_line = agent_config::record_for_event(
            event,
            claude_session_id,
            stdin_payload,
            adapter_name,
            &ctx,
            crate::paths::state_dir(),
        );
        let (session_notice, session_phases) = if event == HookEvent::SessionStart {
            session_notices::session_start_notices(rt, &config, config_dir, &active, roots_notice)
        } else {
            (None, crate::trace::SessionStartPhases::default())
        };
        // The project notice applies to every event, because the recall of every event runs
        // without its project scope when the lookup fails (#2688).
        let health_notice = [session_notice, memory_notice, runtime_failure]
            .into_iter()
            .flatten()
            .reduce(|acc, next| format!("{acc}\n{next}"));
        let spawn_us = if resumes_checkpoints(
            event,
            stdin_payload,
            std::env::var_os(crate::consolidation::CHILD_GUARD_ENV).as_deref(),
        ) {
            // After the notice, so the proxy it may restart is up (#2396).
            let t_start = std::time::Instant::now();
            resume_checkpoints(checkpoint::spawner_state_dir().as_deref());
            Some(u64::try_from(t_start.elapsed().as_micros()).unwrap_or(u64::MAX))
        } else {
            None
        };
        let t_chunk = std::time::Instant::now();
        let out = match rt {
            Some(rt) => rt.block_on(async {
                let mut out = String::new();
                let mut memory = String::new();
                // llmenv's omission notice rides in `notes`, not in the ICM memory (#2764).
                let mut memory_notes = String::new();
                // #866: skip the Store action (and its dedup-snapshot rewrite,
                // below) when SessionEnd's chunk is unchanged — but still run the
                // rest of this block (session-log capture) unconditionally.
                // `session_end_unchanged` is only ever `true` for `SessionEnd`
                // (see its computation above), so it alone is the store-skip
                // condition; no need to re-check the event here too.
                if let Some(client) = &client
                    && !session_end_unchanged
                    && !memory_skipped
                {
                    let wake_call = (!continues_session(stdin_payload)).then_some(&wake);
                    let actions = global_topics::turn_actions(
                        dispatch(event, &tag_queries, &bundle_queries, &tag_ranks, wake_call),
                        &query,
                        settings.as_ref().map_or(&[], |s| &s.global_topics),
                    );
                    // Use minimal chunk for storage to avoid duplication. (#1792)
                    let store_content = store_content_for_event(event, &chunk, &storage_chunk);
                    memory = match run_event_memory(MemoryCall {
                        event,
                        client,
                        settings: settings.as_ref(),
                        session_id: claude_session_id,
                        state_dir: crate::paths::state_dir()
                            .inspect_err(|e| {
                                tracing::error!("no state dir, adaptive recall falls back: {e}");
                            })
                            .ok(),
                        payload: stdin_payload,
                        actions,
                        scope: scope_recall_actions(&tag_queries, &bundle_queries, &tag_ranks),
                        wake: wake.clone(),
                        query: &query,
                        store_content,
                    })
                    .await
                    {
                        Ok(text) => {
                            memory_notes = text.notes;
                            text.memory
                        }
                        // The health notice below still has to reach the agent. So does this error:
                        // the notice may be about another server, and the agent would otherwise
                        // read an empty memory block as "nothing to recall".
                        Err(e) if health_notice.is_some() => {
                            eprintln!("llmenv: memory {event} skipped: {e}");
                            out = format!(
                                "llmenv: memory {event} failed: {}\n",
                                mcp_health::tidy_reason(&format!("{e:#}"))
                            );
                            String::new()
                        }
                        Err(e) => return Err(e),
                    };

                    // PostToolUse WebFetch/WebSearch: auto-store fetched content in ICM
                    // with fast-falloff memory (topic: web-fetch, importance: low) so it
                    // survives session compactions but decays quickly. (#579)
                    if stores_web_fetch(event) {
                        // Detached: process-group-detached and outlives us regardless.
                        let _detached_child = handle_web_fetch_post_tool_use(stdin_payload);
                    }
                }
                if let Some(notice) = &health_notice {
                    out = if out.is_empty() {
                        notice.clone()
                    } else {
                        format!("{notice}\n{out}")
                    };
                }
                if let Some(line) = &agent_line {
                    out = if out.is_empty() {
                        line.clone()
                    } else {
                        format!("{line}\n{out}")
                    };
                }
                let t_session_log = std::time::Instant::now();
                run_session_log(event, &session_log, stdin_payload).await;
                session_log_us =
                    Some(std::time::Duration::as_micros(&t_session_log.elapsed()) as u64);

                // Update dedup snapshot *after* the store succeeds (R3). Writing before
                // the store call means a transient MCP failure leaves the snapshot ahead
                // of reality — the next SessionEnd sees the chunk as unchanged and skips
                // the store, permanently losing the memory. (#594 code review)
                // Store the minimal chunk in the dedup snapshot to match what was stored. (#1792)
                if let Some(dedup_path) =
                    dedup_write_target(session_end_dedup_path.as_deref(), session_end_unchanged)
                {
                    // Logged, not returned: a `?` here would drop the notices and the memory
                    // output built above (#2719). The next SessionEnd may store the chunk again.
                    if let Err(e) =
                        crate::paths::write_owner_only_atomic(dedup_path, storage_chunk.as_bytes())
                    {
                        tracing::error!(
                            "session-end dedup snapshot not saved to {}, the next SessionEnd may store the chunk again: {e:#}",
                            dedup_path.display()
                        );
                    }
                }

                // #231: append the task-tracker Stop reminder. Only reached here when
                // session-log also wants Stop (the log_cfg.any_sink_enabled() case
                // above already short-circuited before this point otherwise) — so
                // this never displaces run_session_log, it just adds to `out`.
                // A missing state directory drops only this reminder, logged. The `?` form would
                // drop the output built above (#2719).
                let stop_state_dir = match (event == HookEvent::Stop && task_tracker_enabled)
                    .then(crate::paths::state_dir)
                {
                    Some(Ok(dir)) => Some(dir),
                    Some(Err(e)) => {
                        tracing::error!(
                            "stop reminder skipped, the state directory is unavailable: {e}"
                        );
                        None
                    }
                    None => None,
                };
                if let Some(state_dir) = stop_state_dir {
                    let reminder = resolve_stop_reminder(
                        &state_dir,
                        claude_session_id,
                        &config,
                        stdin_payload,
                    );
                    append_section(&mut out, &reminder);
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
                if let Some(text) = &nudge_text {
                    append_read_once_result(&mut out, text);
                }

                let notes = join_notes(Some(memory_notes.as_str()), &out);
                Ok::<HookText, anyhow::Error>(HookText { memory, notes })
            }),
            None => Ok(HookText::default()),
        };
        // After the store, so the child's recall can see this session's chunk,
        // and before `?`, so a failed dedup write or log step does not skip it.
        if client.is_some() {
            maybe_start_consolidation(
                event,
                &config,
                config_dir,
                &active,
                post_session_consolidation,
            );
        }
        let out = out?;
        let mut final_phases = session_phases;
        final_phases.cbm_roots = cbm_roots_took;
        let t_end = std::time::Instant::now();
        crate::trace::emit_trace_timing(
            crate::trace::HookTiming::new(t0, t_config)
                .through_scope(t_scope)
                .through_mcp(t_chunk, t_end)
                .with_events(final_phases, session_log_us, checkpoint_us, spawn_us),
        );
        Ok(out)
    })();

    match pipeline_result {
        Ok(out) => Ok(out),
        Err(e) => {
            // #1128: this is also a return point. The memory-client and network failures
            // (#867) are caught fail-soft inside the pipeline, so this covers the rarer errors
            // that reach here, such as a tag or bundle validation failure. t_scope, t_chunk,
            // and t_end are lost with the closure that computed them.
            let text =
                recover_from_pipeline_error(event, e, [pre_tool_text, turn_text, nudge_text])?;
            crate::trace::emit_trace_timing(crate::trace::HookTiming::new(t0, t_config));
            Ok(text)
        }
    }
}

/// Keeps the hook text that was computed before the pipeline failed.
///
/// A pipeline error must not drop a PreToolUse decision (#867), the turn digest, or a nudge.
/// A nudge is recorded as delivered when it is computed, so dropping it here would lose it
/// for good (#2730). Without any computed text, the error is returned unchanged.
fn recover_from_pipeline_error(
    event: HookEvent,
    error: anyhow::Error,
    precomputed: [Option<String>; 3],
) -> anyhow::Result<HookText> {
    let Some(recovered) = join_precomputed(precomputed) else {
        return Err(error);
    };
    // The default `RUST_LOG` hides `warn!`, so the error also goes to stderr. The agent gets it
    // as a notice, so the kept text does not hide the skipped memory step.
    eprintln!("llmenv: memory {event} skipped: {error}");
    crate::agent_notice::record(memory_skipped_notice(event, &error));
    // The stderr line and the agent notice above already report this, so the log is debug only.
    tracing::debug!(
        error = %error,
        "hook-run: pipeline failed after hook text was already computed; \
         returning it instead of silently dropping it"
    );
    Ok(HookText::from_notes(recovered))
}

/// The last stored SessionEnd chunk, or `None`. A missing file is the normal first run, so only
/// another read error is logged.
fn read_dedup_snapshot(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .inspect_err(|e| {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::error!("failed to read dedup cache {}: {e}", path.display());
            }
        })
        .ok()
}

/// The agent notice for a hook run whose memory step failed.
fn memory_skipped_notice(event: impl std::fmt::Display, err: &anyhow::Error) -> String {
    format!(
        "llmenv: memory {event} skipped: {}. Fix: run `llmenv doctor` to check the memory server.",
        mcp_health::tidy_reason(&format!("{err:#}"))
    )
}

/// Joins the precomputed hook texts in the order the success path appends them.
///
/// Returns `None` when no text was computed, so the caller can tell "nothing to keep" from
/// "a text that is empty".
fn join_precomputed(precomputed: [Option<String>; 3]) -> Option<String> {
    if precomputed.iter().all(Option::is_none) {
        return None;
    }
    let mut out = String::new();
    for text in precomputed.into_iter().flatten().filter(|t| !t.is_empty()) {
        append_read_once_result(&mut out, &text);
    }
    Some(out)
}

/// Run one event's ordered memory actions and concatenate their text output.
///
/// `TurnStart` recall is capped and deduplicated by record, most specific first, so the
/// injected context stays under the size Claude Code inlines (#2159). See
/// [`recall::run_with_budget`].
async fn run_memory_actions(
    client: &McpHttpClient,
    actions: Vec<Action>,
    query: &str,
    chunk: &str,
) -> HookText {
    let (text, budget) = recall::run_with_budget(actions, |action| async move {
        action.run(client, query, chunk).await
    })
    .await;
    if let Some(line) = budget.trace_line(crate::trace::enabled()) {
        crate::trace::emit_trace_line(&line);
    }
    HookText {
        memory: text,
        notes: budget.notice().unwrap_or_default(),
    }
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
                let log_path = crate::session_log::resolve_file_path(cfg.file_path())
                    .inspect_err(|e| {
                        tracing::warn!(
                            "session_log reaper: cannot resolve the session log path, so \
                             retention is skipped: {e:#}. Fix: set HOME or session_log.file.path."
                        );
                    })
                    .ok();
                if let Some(p) = log_path.as_ref() {
                    crate::session_log::reap_session_log(p, days);
                }
            }
            // SessionStart: once per launch, worth revalidating (#1090).
            ensure_transcript_session(cfg, client, csid, ctx, state_path, true).await
        }
        (_, Some(csid)) => state_path.and_then(|p| {
            let now = session_state::unix_now();
            match state::link_at(p, csid, now) {
                Ok(state::Link::Linked(id)) => Some(id),
                Ok(state::Link::Pending { since }) => {
                    transcript_link::note_pending(since, now);
                    None
                }
                Ok(state::Link::Missing) => None,
                Err(e) => {
                    // The engine drops a hook's stderr, so the agent hears it here (#2834).
                    tracing::error!("ICM transcript skipped for this event: {e:#}");
                    crate::agent_notice::record(notice::session_map_unreadable_notice(&e));
                    None
                }
            }
        }),
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
    /// `features.memory` entry's hook settings (#1216, #2249).
    Active {
        url: String,
        settings: MemoryHookSettings,
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

    /// The active entry's hook settings (#1216, #2249). `None` for every
    /// non-[`MemoryEndpoint::Active`] variant.
    fn settings(&self) -> Option<&MemoryHookSettings> {
        match self {
            Self::Active { settings, .. } => Some(settings),
            _ => None,
        }
    }

    /// Consume into `(url, settings)`, erroring exactly as [`Self::into_url`] —
    /// `into_url` itself keeps its existing signature since it has several
    /// other callers that don't need the settings.
    fn into_url_and_settings(self) -> anyhow::Result<(String, MemoryHookSettings)> {
        let settings = self.settings().cloned().unwrap_or_else(default_memory_hook);
        Ok((self.into_url()?, settings))
    }
}

/// The settings of a `features.memory` entry that sets neither hook field.
fn default_memory_hook() -> MemoryHookSettings {
    MemoryHookSettings {
        wakeup_max_tokens: None,
        adaptive_recall: true,
        default_type: None,
        default_importance: None,
        global_topics: crate::config::Memory::default().global_topics,
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
/// Top-level and firing-bundle `features.memory` and `host` entries, merged the way
/// `build_manifest` merges them.
pub(crate) struct MergedMemory<'a> {
    firing: Vec<&'a crate::config::Bundle>,
    bundle_refs: Vec<crate::merge::BundleRef>,
    pub(crate) memory: Vec<crate::config::Memory>,
    pub(crate) host: std::collections::BTreeMap<String, crate::config::HostEntry>,
}

/// Merge the memory and host entries for the active scopes. Endpoint resolution and
/// `llmenv doctor` both use it, so neither one misses a bundle-declared memory server.
///
/// # Errors
/// A firing bundle's `bundle.yaml` cannot be read or merged.
pub(crate) fn merged_memory<'a>(
    config: &'a crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<MergedMemory<'a>> {
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

    let mut memory: Vec<crate::config::Memory> = top_memory
        .iter()
        .chain(bundle_memory.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut memory);

    // Merged host: bundle contributions first, top-level overwrites (same as build_manifest).
    let mut host = bundle_host;
    for (k, v) in &config.host {
        host.insert(k.clone(), v.clone());
    }
    Ok(MergedMemory {
        firing,
        bundle_refs,
        memory,
        host,
    })
}

pub(crate) fn memory_url(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<MemoryEndpoint> {
    let MergedMemory {
        firing,
        bundle_refs,
        memory: all_memory,
        host: all_host,
    } = merged_memory(config, config_dir, active)?;

    // Full `active.tags` (not `non_project_tags()`) on purpose: this resolves
    // the memory backend for the *live* hook-run session, which is legitimately
    // project-aware — unlike `build_manifest`'s static host-cache render, which
    // must exclude project-only tags (#696/#979). Do not "align" this with
    // build_manifest's host_tags; that would break recall in project scopes.
    let resolved = resolve_mcps(&config.mcp, &all_memory, &all_host, &active.tags)
        .map_err(|e| annotate_resolve_error(e, config, config_dir, active))?;
    let matched = resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some((url, m.memory_hook)),
        _ => None,
    });
    Ok(match matched {
        Some((url, settings)) => MemoryEndpoint::Active {
            url,
            settings: settings.unwrap_or_else(default_memory_hook),
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
                .inspect_err(|e| {
                    crate::agent_notice::record(format!(
                        "llmenv could not read bundle {} to check its suppressed capabilities: \
                         {e:#}. Fix: check the bundle's bundle.yaml.",
                        r.name
                    ));
                })
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
            let cache_dir = match crate::paths::expand_tilde(&config.cache.cache_dir) {
                Ok(dir) => dir,
                Err(e) => {
                    // `error!`, not `warn!`: the default filter hides `warn!` (#1139). A skipped
                    // lookup is a cache miss, and the merge runs without the cache.
                    tracing::error!("merge cache lookup skipped: {e:#}");
                    return None;
                }
            };
            let cache_root = std::path::PathBuf::from(cache_dir);
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

/// Apply default memory type/importance markers from the resolved memory
/// entry's `settings` (R1, R3). `None` (no backend resolved) leaves the chunk
/// unchanged.
///
/// If the chunk already contains an `<!-- llmenv-type: -->` or
/// `<!-- llmenv-importance: -->` marker, the inline value takes precedence and
/// no default is appended. Otherwise the entry's `default_type` /
/// `default_importance` are appended as markers at the end of the chunk.
///
/// ponytail: `type_importance` per-type overrides are not yet applied here —
/// they will be resolved when the Store action runs against the ICM backend.
fn apply_memory_config_defaults(
    mut chunk: String,
    settings: Option<&MemoryHookSettings>,
) -> String {
    let Some(settings) = settings else {
        return chunk;
    };

    if !chunk.contains("<!-- llmenv-type:")
        && let Some(ty) = &settings.default_type
    {
        chunk.push_str(&format!("\n<!-- llmenv-type: {} -->", ty.as_marker_str()));
    }

    if !chunk.contains("<!-- llmenv-importance:")
        && let Some(imp) = &settings.default_importance
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

/// Where codebase-memory-mcp's index cache lives for `cm`/`state_dir`: the
/// configured `index_path` override, or `state_dir/codebase-memory` by
/// default. Single source of truth for both the spawned child's
/// `CBM_CACHE_DIR` and the indexer's own diagnostic log (#1091), so they
/// can't drift apart.
///
/// # Errors
/// `index_path` cannot be expanded, for example a leading `~` with `HOME` unset.
pub(crate) fn codebase_memory_cache_dir(
    cm: &crate::config::CodebaseMemory,
    state_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    Ok(cache_dir_of(expanded_index_path(cm)?.as_deref(), state_dir))
}

/// The `index_path` of `cm` with a leading `~` expanded. The loader already does this (#2651),
/// so this call protects a value that reaches the indexer without the loader.
///
/// # Errors
/// `index_path` cannot be expanded, for example a leading `~` with `HOME` unset.
fn expanded_index_path(cm: &crate::config::CodebaseMemory) -> anyhow::Result<Option<String>> {
    cm.index_path
        .as_deref()
        .map(crate::paths::expand_tilde)
        .transpose()
}

/// The cache folder for an already-expanded `index_path`, or the default under `state_dir`.
fn cache_dir_of(index_path: Option<&str>, state_dir: &std::path::Path) -> std::path::PathBuf {
    index_path
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
/// `harden_dir` is forwarded to `open_bounded_log`, which does the 0700
/// hardening itself (#1196) — pass `false` when `log_path`'s directory may be
/// shared with a process running under a different uid (e.g. a
/// user-configured `index_path`). `context` names the caller in the
/// debug-level "log unavailable" message, since that message is shared across
/// callers with different failure consequences (#1141). No `max_bytes`
/// parameter: every caller bounds to the same [`BOUNDED_LOG_MAX_BYTES`] now
/// that the two call sites' previously-distinct constants turned out to be
/// identical (#1141) — a parameter every caller passes the same value for
/// isn't a real degree of freedom.
fn redirect_stderr_to_bounded_log(
    cmd: &mut std::process::Command,
    log_path: &std::path::Path,
    harden_dir: bool,
    context: &str,
) {
    cmd.stderr(std::process::Stdio::null());
    match crate::mcp::proxy::open_bounded_log(log_path, BOUNDED_LOG_MAX_BYTES, harden_dir) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            tracing::warn!(
                "{context}: log unavailable ({e:#}); the child's stderr is discarded. Check that the log path is a regular file owned by you."
            );
        }
    }
}

/// Send a detached child's stderr to the shared bounded log instead of discarding it (#1133, the
/// same remedy as #1091). `log` is the resolved [`detached_child_log_path`].
///
/// `Stdio::null()` leaves such a child with no report channel whatsoever: its own `tracing`
/// events go to a fmt layer writing to that same null stderr, so a failure is discarded twice over.
pub(crate) fn redirect_stderr_to_detached_log(
    cmd: &mut std::process::Command,
    log: anyhow::Result<std::path::PathBuf>,
) {
    match log {
        // Always state_dir-rooted, so `harden_dir: true` is safe.
        Ok(path) => redirect_stderr_to_bounded_log(cmd, &path, true, "detached child"),
        Err(e) => {
            cmd.stderr(std::process::Stdio::null());
            tracing::warn!(
                "detached child: cannot resolve log path ({e:#}); the child's stderr is discarded"
            );
        }
    }
}

/// The SessionEnd dedup snapshot path. Other events have none. A missing state directory skips
/// the snapshot and logs, because an error return would drop the notices built above (#2719).
fn dedup_snapshot_path(
    event: HookEvent,
    state_dir: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if event != HookEvent::SessionEnd {
        return None;
    }
    match state_dir() {
        Ok(dir) => Some(dir.join(crate::paths::HOOK_STORE_CHUNK)),
        Err(e) => {
            tracing::error!(
                "session-end dedup skipped, the state directory is unavailable, so the chunk is stored again: {e}"
            );
            None
        }
    }
}

/// Whether a PostToolUse event may store fetched web content in ICM (#579).
fn stores_web_fetch(event: HookEvent) -> bool {
    event == HookEvent::PostToolUse
}

/// The dedup snapshot to write after a store. A chunk the store already matched is not written
/// again, so the snapshot keeps its earlier content.
fn dedup_write_target(path: Option<&std::path::Path>, unchanged: bool) -> Option<&std::path::Path> {
    path.filter(|_| !unchanged)
}

/// Append `section` to `out` on its own line. An empty section adds nothing.
fn append_section(out: &mut String, section: &str) {
    if section.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(section);
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::expect_fun_call)]
mod pipeline_recovery_tests {
    use super::{
        HookEvent, HookText, deny_drops_memory, join_notes, join_precomputed, read_dedup_snapshot,
        recover_from_pipeline_error,
    };
    use proptest::strategy::Strategy as _;

    fn failure() -> anyhow::Error {
        anyhow::anyhow!("tag `bad tag` is not valid")
    }

    #[test]
    fn a_failed_pipeline_keeps_a_computed_nudge() {
        // #2730: the nudge is recorded as delivered when it is computed, so it must reach the agent.
        let text = recover_from_pipeline_error(
            HookEvent::PostToolUse,
            failure(),
            [None, None, Some("nudge: wait for the user".to_string())],
        )
        .expect("a computed nudge must survive the pipeline error");
        assert_eq!(
            text,
            HookText::from_notes("nudge: wait for the user".to_string())
        );
        let notice = crate::agent_notice::current().unwrap_or_default();
        assert!(
            notice.contains("llmenv: memory") && notice.contains("bad tag"),
            "the skipped memory step must reach the agent: {notice}"
        );
        assert!(
            notice.contains("memory PostToolUse skipped") || notice.contains("skipped: tag"),
            "the skipped memory step must reach the agent: {notice}"
        );
    }

    #[test]
    fn a_failed_pipeline_with_no_computed_text_returns_the_error() {
        let err =
            recover_from_pipeline_error(HookEvent::PostToolUse, failure(), [None, None, None])
                .expect_err("nothing to keep, so the error must propagate");
        assert!(err.to_string().contains("bad tag"), "got: {err}");
    }

    #[test]
    fn a_deny_drops_only_memory_that_was_computed() {
        let with_memory = HookText {
            memory: "recalled".to_string(),
            notes: "__DENY__:why".to_string(),
        };
        assert!(deny_drops_memory(&with_memory));
        assert!(!deny_drops_memory(&HookText::from_notes(
            "__DENY__:why".to_string()
        )));
    }

    #[test]
    fn join_notes_joins_only_the_parts_that_have_text() {
        assert_eq!(join_notes(Some("notice"), "note"), "notice\n\nnote");
        assert_eq!(join_notes(Some("notice"), ""), "notice");
        assert_eq!(join_notes(Some(""), "note"), "note");
        assert_eq!(join_notes(None, "note"), "note");
        assert_eq!(join_notes(None, ""), "");
    }

    #[test]
    fn precomputed_texts_join_in_success_path_order() {
        let joined = join_precomputed([
            Some("pre".to_string()),
            Some("turn".to_string()),
            Some("nudge".to_string()),
        ]);
        assert_eq!(joined.as_deref(), Some("pre\nturn\nnudge"));
    }

    #[test]
    fn an_empty_precomputed_text_adds_no_separator() {
        let joined = join_precomputed([Some(String::new()), None, Some("nudge".to_string())]);
        assert_eq!(joined.as_deref(), Some("nudge"));
    }

    #[test]
    fn a_deny_precomputed_text_keeps_its_prefix() {
        // Matches the success path: the deny marker stays first, and later text appends after it.
        let joined = join_precomputed([
            Some("__DENY__:already read".to_string()),
            Some("turn".to_string()),
            None,
        ]);
        assert_eq!(joined.as_deref(), Some("__DENY__:already read\nturn"));
    }

    #[test]
    fn the_dedup_snapshot_reads_the_stored_chunk_and_a_missing_file_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session_end_dedup");
        assert_eq!(
            read_dedup_snapshot(&path),
            None,
            "a first run has no snapshot"
        );
        std::fs::write(&path, "stored chunk").expect("write snapshot");
        assert_eq!(read_dedup_snapshot(&path).as_deref(), Some("stored chunk"));
    }

    /// A slot value: absent, empty, plain text, or a deny marker.
    fn slot() -> impl proptest::strategy::Strategy<Value = Option<String>> {
        proptest::prop_oneof![
            proptest::strategy::Just(None),
            proptest::strategy::Just(Some(String::new())),
            "[a-z]{1,8}".prop_map(Some),
            "[a-z]{1,8}".prop_map(|t| Some(format!("__DENY__:{t}"))),
        ]
    }

    proptest::proptest! {
        #[test]
        fn joined_output_follows_the_success_path_rules(a in slot(), b in slot(), c in slot()) {
            let slots = [a, b, c];
            let joined = join_precomputed(slots.clone());
            proptest::prop_assert_eq!(joined.is_none(), slots.iter().all(Option::is_none));
            let texts: Vec<&str> = slots
                .iter()
                .flatten()
                .map(String::as_str)
                .filter(|t| !t.is_empty())
                .collect();
            let joined = joined.unwrap_or_default();
            match texts.iter().rposition(|t| t.starts_with("__DENY__:")) {
                // The last deny replaces what came before it; later texts append after it.
                Some(last_deny) => proptest::prop_assert_eq!(joined, texts[last_deny..].join("\n")),
                None => proptest::prop_assert_eq!(joined, texts.join("\n")),
            }
        }
    }
}

#[cfg(test)]
mod write_hook_line_tests {
    use super::write_hook_line;
    use std::io::{Error, ErrorKind, Write};

    /// A writer that fails every write with the given kind.
    struct Failing(ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(Error::from(self.0))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn empty_output_writes_nothing_and_reports_nothing() {
        let mut buf = Vec::new();
        assert!(write_hook_line(&mut buf, "").is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn output_is_written_as_one_line() {
        let mut buf = Vec::new();
        assert!(write_hook_line(&mut buf, "context").is_none());
        assert_eq!(buf, b"context\n");
    }

    #[test]
    fn a_closed_pipe_is_not_reported() {
        assert!(write_hook_line(&mut Failing(ErrorKind::BrokenPipe), "x").is_none());
    }

    #[test]
    fn any_other_write_error_is_reported() {
        let kind =
            write_hook_line(&mut Failing(ErrorKind::PermissionDenied), "x").map(|e| e.kind());
        assert_eq!(kind, Some(ErrorKind::PermissionDenied));
    }
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
///
/// Returns the notice for the agent when the index is skipped. The log alone does not reach the
/// agent, so the skip is also reported at session start.
fn trigger_codebase_memory_index(
    project_root: &std::path::Path,
    cm: &crate::config::CodebaseMemory,
    state_dir: &std::path::Path,
) -> Option<String> {
    trigger_index_with(project_root, cm, state_dir, || {
        crate::path_lookup::resolve_program("codebase-memory-mcp")
    })
}

/// [`trigger_codebase_memory_index`] with the program lookup given, so a test needs no indexer.
fn trigger_index_with(
    project_root: &std::path::Path,
    cm: &crate::config::CodebaseMemory,
    state_dir: &std::path::Path,
    find_program: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> Option<String> {
    // A checkpointed run goes through `llmenv cbm-index-run`, which observes the indexer's exit
    // status. Without a checkpoint the indexer starts directly, as before (#2396).
    let index_path = match expanded_index_path(cm) {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("codebase-memory index skipped: cannot expand index_path: {e:#}");
            return Some(index_skipped_notice(&format!(
                "cannot expand index_path: {e:#}"
            )));
        }
    };
    let program = match find_program() {
        Ok(program) => program,
        Err(e) => {
            tracing::error!("codebase-memory index skipped: {e:#}");
            return Some(index_skipped_notice(&format!("{e:#}")));
        }
    };
    let cache_dir = cache_dir_of(index_path.as_deref(), state_dir);
    let result_path = index_result_path(&cache_dir, project_root);
    let inputs = detached_cbm::IndexInputs {
        project_root: project_root.display().to_string(),
        index_path: index_path.clone(),
        mem_budget_mb: cm.mem_budget_mb,
        result_path: Some(result_path.display().to_string()),
    };
    // Spawns this same binary as a detached child. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = std::env::current_exe().ok();
    let checkpoint = begin_index_checkpoint(state_dir, &inputs, exe.is_some());
    // The wrapper is used only with both a checkpoint and an executable (`index_job_command`).
    let wrapped = uses_wrapper(checkpoint.as_deref(), exe.as_deref());
    let mut cmd = index_job_command(checkpoint.as_deref(), exe, || {
        index_command(
            &program,
            project_root,
            index_path.as_deref(),
            cm.mem_budget_mb,
        )
    });
    let log_path = cache_dir.join("index.log");
    // Only the default cache dir (under llmenv's own state tree) gets
    // hardened to 0700. A user-configured `index_path` (#1196) can be shared
    // with a codebase-memory-mcp process running under a different uid —
    // forcing it to 0700 would silently break that sharing with an EACCES on
    // the next run.
    let harden_dir = cm.index_path.is_none();
    redirect_stderr_to_bounded_log(
        &mut cmd,
        &log_path,
        harden_dir,
        "codebase-memory-mcp index_repository",
    );
    // Without the wrapper the indexer starts here, so its stdout goes to the result file here. The
    // wrapper does the same for the indexer it starts. The log redirect above made the directory.
    direct_index_stdout(&mut cmd, wrapped, &result_path);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    if let Err(e) = crate::trace::spawn_counted("cbm:index", &mut cmd) {
        tracing::error!("codebase-memory-mcp index_repository: failed to spawn: {e}");
        return Some(index_skipped_notice(&format!(
            "the indexer did not start: {e}"
        )));
    }
    None
}

/// Opens the index checkpoint only when the indexer can be wrapped to settle it. Without the
/// executable path the wrapper cannot run, so the checkpoint would stay open, and every later
/// SessionStart would run the index again (#2696). The index then runs unchecked, and the error
/// log says why.
fn begin_index_checkpoint(
    state_dir: &std::path::Path,
    inputs: &detached_cbm::IndexInputs,
    exe_known: bool,
) -> Option<std::path::PathBuf> {
    if !exe_known {
        tracing::error!(
            "codebase-memory index runs without a checkpoint: the executable path is unknown"
        );
        return None;
    }
    checkpoint::begin(Some(state_dir), checkpoint::JobKind::CbmIndex, inputs, None)
}

/// The session-start line for an index that did not run. `why` names the cause and the fix.
fn index_skipped_notice(why: &str) -> String {
    format!("codebase-memory: index skipped, {why}\n")
}

/// Whether this hook run starts the unfinished background jobs again: a `SessionStart` that is
/// not the continuation of an earlier conversation, outside the consolidation child (#2396).
fn resumes_checkpoints(
    event: HookEvent,
    payload: &serde_json::Value,
    consolidation_guard: Option<&std::ffi::OsStr>,
) -> bool {
    event == HookEvent::SessionStart
        && !continues_session(payload)
        && !is_consolidation_child(consolidation_guard)
}

/// Start the detached jobs whose checkpoints are stale: work that an earlier session started and
/// never finished (#2396). Returns the count of resumed jobs. Fail-soft: a failure is logged at
/// debug level and the session goes on.
fn resume_checkpoints(state_dir: Option<&std::path::Path>) -> usize {
    resume_checkpoints_with(state_dir, respawn_job)
}

/// [`resume_checkpoints`] with the job starter injected, so a test starts no process.
fn resume_checkpoints_with(
    state_dir: Option<&std::path::Path>,
    spawn: impl FnMut(&checkpoint::Checkpoint, &std::path::Path) -> anyhow::Result<()>,
) -> usize {
    let Some(state_dir) = state_dir else {
        return 0;
    };
    let resumed = checkpoint::resume_pending(state_dir, checkpoint::now_secs(), spawn);
    tracing::debug!("resumed {resumed} unfinished background job(s)");
    resumed
}

/// True for the event that ends a session and so starts consolidation.
/// Claude Code's `SessionEnd` arrives as `session_end`, and no adapter emits
/// `post_session`, so consolidation keyed on `PostSession` alone never ran
/// (#2355). One event per session end means one child per session end.
fn starts_consolidation(event: HookEvent) -> bool {
    matches!(event, HookEvent::SessionEnd | HookEvent::PostSession)
}

/// Start the detached consolidation child when `event` ends the session and
/// the active memory entry enables consolidation. The check reads the same
/// merged memory list as endpoint resolution, so a bundle-declared entry
/// counts. Fail-soft: a merge error is logged and skips consolidation.
///
/// `spawn` is [`post_session_consolidation`] in production; tests pass a
/// counter, because a detached child is not visible from inside the test.
fn maybe_start_consolidation(
    event: HookEvent,
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    spawn: fn(),
) {
    let guard = std::env::var_os(crate::consolidation::CHILD_GUARD_ENV);
    if !starts_consolidation(event) || is_consolidation_child(guard.as_deref()) {
        return;
    }
    match merged_memory(config, config_dir, active) {
        Ok(merged) => {
            if crate::consolidation::active_consolidation(&merged.memory, &active.tags).is_some() {
                spawn();
            }
        }
        Err(e) => tracing::error!("consolidation: cannot read merged memory entries: {e:#}"),
    }
}

/// True when this `hook-run` runs inside the `claude -p` child that
/// consolidation starts. Such a run starts no consolidation, so the child
/// cannot loop even if its hooks run (#2355). Only the consolidation start
/// checks it: a stray value must not turn off PreToolUse denies or the
/// session log.
fn is_consolidation_child(guard: Option<&std::ffi::OsStr>) -> bool {
    guard.is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use crate::trace::{HookTiming, hook_marker};
    use serde_json::json;

    #[test]
    fn a_notice_with_no_channel_is_logged_at_error_level() {
        let logs = crate::test_log_capture::capture_logs(|| {
            log_notice_without_channel("", Some("the session map is locked"), "session_end");
        });
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("the session map is locked"), "{logs}");
    }

    #[test]
    fn memory_with_no_channel_is_logged_and_memory_with_one_is_not() {
        let dropped = crate::test_log_capture::capture_logs(|| {
            log_memory_without_channel("", "project memory text", "session_start");
        });
        assert!(
            dropped.contains("ERROR") && dropped.contains("19 bytes"),
            "{dropped}"
        );
        let delivered = crate::test_log_capture::capture_logs(|| {
            log_memory_without_channel("{}", "project memory text", "session_start");
            log_memory_without_channel("", "  ", "session_start");
        });
        assert!(delivered.is_empty(), "{delivered}");
    }

    #[test]
    fn a_notice_with_a_channel_is_not_logged_here() {
        let logs = crate::test_log_capture::capture_logs(|| {
            log_notice_without_channel("{}", Some("notice"), "turn_start");
        });
        assert!(logs.is_empty(), "{logs}");
    }

    #[test]
    fn every_listed_hook_event_name_parses_and_round_trips() {
        for name in HOOK_EVENT_NAMES {
            let event: HookEvent = name.parse().unwrap();
            assert_eq!(event.to_string(), name);
        }
        let err = "nope".parse::<HookEvent>().unwrap_err().to_string();
        for name in HOOK_EVENT_NAMES {
            assert!(err.contains(name), "{name} missing from: {err}");
        }
    }

    use super::web_fetch::handle_web_fetch_in;
    use super::*;

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
        "post_tool_batch",
        "post_tool_use_failure",
        "subagent_start",
        "subagent_task",
        "post_model_switch",
    ];

    #[test]
    fn only_session_end_and_post_session_start_consolidation() {
        let starting: Vec<&str> = ALL_HOOK_EVENTS
            .iter()
            .copied()
            .filter(|e| starts_consolidation(HookEvent::from_str(e).unwrap()))
            .collect();
        // Claude Code registers `session_end` once and never `post_session`,
        // so one session end starts one child (#2355).
        assert_eq!(starting, ["session_end", "post_session"]);
    }

    thread_local! {
        static SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn count_spawn() {
        SPAWNS.with(|c| c.set(c.get() + 1));
    }

    /// Spawns for one `event` against a config whose only memory entry
    /// (tag `mem`) has consolidation set to `enabled`.
    fn consolidation_spawns(event: HookEvent, enabled: bool) -> usize {
        let config: crate::config::Config = serde_yaml::from_str(&format!(
            "features:\n  memory:\n    - server_host: h\n      port: 1\n      when: [mem]\n      \
                 consolidation:\n        enabled: {enabled}\n"
        ))
        .unwrap();
        let active = crate::scope::ActiveScopes {
            tags: std::collections::BTreeSet::from(["mem".to_string()]),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        SPAWNS.with(|c| c.set(0));
        maybe_start_consolidation(event, &config, dir.path(), &active, count_spawn);
        SPAWNS.with(std::cell::Cell::get)
    }

    #[test]
    fn session_end_with_consolidation_enabled_spawns_exactly_one_child() {
        assert_eq!(consolidation_spawns(HookEvent::SessionEnd, true), 1);
        assert_eq!(consolidation_spawns(HookEvent::PostSession, true), 1);
    }

    #[test]
    fn no_child_for_other_events_or_disabled_consolidation() {
        assert_eq!(consolidation_spawns(HookEvent::Stop, true), 0);
        assert_eq!(consolidation_spawns(HookEvent::SessionStart, true), 0);
        assert_eq!(consolidation_spawns(HookEvent::SessionEnd, false), 0);
    }

    #[test]
    fn consolidation_run_command_runs_the_consolidation_subcommand() {
        let cmd = consolidation_run_command("/bin/llmenv".into(), None);
        assert_eq!(cmd.get_program(), "/bin/llmenv");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["consolidation-run"]);
    }

    #[test]
    fn each_detached_command_carries_its_checkpoint_path() {
        let path = std::path::Path::new("/s/checkpoints/x.json");
        let args = |cmd: std::process::Command| -> Vec<String> {
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            args(consolidation_run_command("/bin/llmenv".into(), Some(path))),
            ["consolidation-run", "--checkpoint", "/s/checkpoints/x.json"]
        );
        assert_eq!(
            args(icm_store_command("/bin/llmenv".into(), Some(path))),
            ["icm-store", "--checkpoint", "/s/checkpoints/x.json"]
        );
        assert_eq!(
            args(icm_store_command("/bin/llmenv".into(), None)),
            ["icm-store"]
        );
    }

    #[test]
    fn a_web_fetch_writes_a_checkpoint_with_the_store_args() {
        let dir = tempfile::tempdir().unwrap();
        let payload = json!({
            "session_id": "sess-1", "tool_use_id": "tu1", "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"}, "tool_response": "body",
            "cwd": dir.path().join("proj-a").display().to_string(),
        });
        std::fs::create_dir(dir.path().join("proj-a")).unwrap();
        let child = handle_web_fetch_in(Some(dir.path()), &payload);
        let listed = checkpoint::checkpoint_entries_for_test(dir.path());
        assert_eq!(listed.len(), 1, "{listed:?}");
        let (kind, inputs, session) = &listed[0];
        assert_eq!(*kind, checkpoint::JobKind::IcmStore);
        assert_eq!(
            inputs["topic"], "web-fetch-proj-a",
            "the page keeps its project (#2911)"
        );
        assert!(
            !inputs[detached_store::REQUEST_ID_FIELD]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert_eq!(session.as_deref(), Some("sess-1"));
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[test]
    fn consolidation_spawn_writes_a_checkpoint_with_a_run_tag_and_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        post_session_consolidation_in(Some(dir.path()));
        let listed = checkpoint::list(dir.path());
        let [checkpoint::Entry::Ready(_, cp)] = &listed[..] else {
            panic!("expected one checkpoint: {listed:?}");
        };
        assert_eq!(cp.kind, checkpoint::JobKind::Consolidation);
        assert!(!cp.inputs["run_tag"].as_str().unwrap().is_empty());
        let cwd = std::env::current_dir().unwrap().display().to_string();
        assert_eq!(cp.cwd.as_deref(), Some(cwd.as_str()));
        // A second run in the same directory is a second job, not the same file.
        post_session_consolidation_in(Some(dir.path()));
        assert_eq!(checkpoint::list(dir.path()).len(), 2);
    }

    #[test]
    fn only_a_fresh_session_start_outside_the_consolidation_child_resumes_jobs() {
        let fresh = json!({ "source": "startup" });
        let guard = std::ffi::OsStr::new("1");
        assert!(resumes_checkpoints(HookEvent::SessionStart, &fresh, None));
        assert!(!resumes_checkpoints(
            HookEvent::SessionStart,
            &json!({ "source": "resume" }),
            None
        ));
        assert!(!resumes_checkpoints(
            HookEvent::SessionStart,
            &json!({ "source": "fork" }),
            None
        ));
        assert!(!resumes_checkpoints(
            HookEvent::SessionStart,
            &fresh,
            Some(guard)
        ));
        assert!(!resumes_checkpoints(HookEvent::SessionEnd, &fresh, None));
        assert!(!resumes_checkpoints(HookEvent::PostToolUse, &fresh, None));
    }

    #[test]
    fn the_resume_step_runs_stale_jobs_through_the_starter_and_nothing_without_a_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale =
            checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({"a": 1}), None);
        stale.started_at = 1;
        let path = checkpoint::write(dir.path(), &stale).unwrap().unwrap();
        let mut started = Vec::new();
        let resumed = resume_checkpoints_with(Some(dir.path()), |c, p| {
            started.push((c.inputs.clone(), p.to_path_buf()));
            Ok(())
        });
        assert_eq!(resumed, 1);
        assert_eq!(started, vec![(json!({"a": 1}), path.clone())]);
        assert_eq!(checkpoint::load(&path).unwrap().attempts, 2);
        assert_eq!(resume_checkpoints_with(None, |_, _| Ok(())), 0);
    }

    #[test]
    fn the_index_job_runs_through_the_wrapper_only_with_a_checkpoint_and_an_executable() {
        let direct = || std::process::Command::new("codebase-memory-mcp");
        let path = std::path::Path::new("/s/checkpoints/cbm-index-x.json");
        let args = |cmd: &std::process::Command| -> Vec<String> {
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let wrapped = index_job_command(Some(path), Some("/bin/llmenv".into()), direct);
        assert_eq!(wrapped.get_program(), "/bin/llmenv");
        assert_eq!(
            args(&wrapped),
            [
                "cbm-index-run",
                "--checkpoint",
                "/s/checkpoints/cbm-index-x.json"
            ]
        );
        assert_eq!(
            index_job_command(None, Some("/bin/llmenv".into()), direct).get_program(),
            "codebase-memory-mcp"
        );
        assert_eq!(
            index_job_command(Some(path), None, direct).get_program(),
            "codebase-memory-mcp"
        );
    }

    #[test]
    fn the_resume_step_starts_a_stale_job_in_its_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut stale = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({}), None);
        stale.started_at = 1;
        stale.cwd = Some(work.path().display().to_string());
        let path = checkpoint::write(dir.path(), &stale).unwrap().unwrap();
        // The child is this test binary with arguments it rejects, so it exits at once.
        resume_checkpoints(Some(dir.path()));
        assert_eq!(checkpoint::load(&path).unwrap().attempts, 2);
    }

    #[test]
    fn the_resume_step_skips_the_index_job_and_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = checkpoint::Checkpoint::new(
            checkpoint::JobKind::CbmIndex,
            json!({"project_root": "/r", "index_path": null}),
            None,
        );
        stale.started_at = 1;
        let path = checkpoint::write(dir.path(), &stale).unwrap().unwrap();
        resume_checkpoints(Some(dir.path()));
        assert_eq!(
            checkpoint::load(&path).unwrap().attempts,
            1,
            "SessionStart starts the indexer itself, so a resume would only count an attempt"
        );
    }

    #[test]
    fn the_dedup_snapshot_is_only_for_session_end_with_a_state_dir() {
        let dir = || Ok(std::path::PathBuf::from("/s"));
        let skipped = || anyhow::bail!("state dir gone");
        let never = || -> anyhow::Result<std::path::PathBuf> { anyhow::bail!("not for Stop") };
        let chunk = std::path::PathBuf::from("/s").join(crate::paths::HOOK_STORE_CHUNK);
        assert_eq!(dedup_snapshot_path(HookEvent::SessionEnd, dir), Some(chunk));
        assert_eq!(dedup_snapshot_path(HookEvent::SessionEnd, skipped), None);
        assert_eq!(dedup_snapshot_path(HookEvent::Stop, never), None);
    }

    #[test]
    fn only_post_tool_use_stores_web_fetch() {
        assert!(stores_web_fetch(HookEvent::PostToolUse));
        assert!(!stores_web_fetch(HookEvent::PreToolUse));
    }

    #[test]
    fn dedup_is_written_only_for_a_changed_chunk_with_a_path() {
        let path = std::path::Path::new("/s/dedup.txt");
        assert_eq!(dedup_write_target(Some(path), false), Some(path));
        assert_eq!(dedup_write_target(Some(path), true), None);
        assert_eq!(dedup_write_target(None, false), None);
    }

    #[test]
    fn a_section_is_appended_on_its_own_line_and_an_empty_one_is_dropped() {
        let mut out = String::new();
        append_section(&mut out, "");
        assert_eq!(out, "");
        append_section(&mut out, "first");
        assert_eq!(out, "first");
        append_section(&mut out, "second");
        assert_eq!(out, "first\nsecond");
    }

    #[test]
    fn only_a_context_losing_session_start_resets_read_state() {
        let with = |source: &str| serde_json::json!({ "source": source });
        assert!(resets_read_state(HookEvent::SessionStart, &with("compact")));
        assert!(resets_read_state(HookEvent::SessionStart, &with("clear")));
        assert!(!resets_read_state(HookEvent::SessionStart, &with("resume")));
        assert!(!resets_read_state(
            HookEvent::SessionStart,
            &serde_json::Value::Null
        ));
        assert!(!resets_read_state(HookEvent::PostToolUse, &with("compact")));
        assert!(!resets_read_state(HookEvent::PreToolUse, &with("clear")));
    }

    #[test]
    fn consolidation_guard_needs_a_non_empty_value() {
        assert!(!is_consolidation_child(None));
        assert!(!is_consolidation_child(Some(std::ffi::OsStr::new(""))));
        assert!(is_consolidation_child(Some(std::ffi::OsStr::new("1"))));
    }

    #[test]
    fn adaptive_applies_only_to_its_events_with_a_valid_session() {
        let on = Some(MemoryHookSettings {
            adaptive_recall: true,
            global_topics: vec![],
            ..MemoryHookSettings::default()
        });
        let off = Some(MemoryHookSettings {
            global_topics: vec![],
            ..MemoryHookSettings::default()
        });
        for event in [
            HookEvent::SessionStart,
            HookEvent::TurnStart,
            HookEvent::PostToolUseFailure,
            HookEvent::SubagentStart,
        ] {
            assert!(uses_adaptive(event, on.as_ref(), Some("s1")), "{event}");
            assert!(!uses_adaptive(event, off.as_ref(), Some("s1")), "{event}");
            assert!(!uses_adaptive(event, on.as_ref(), None), "{event}");
            assert!(
                !uses_adaptive(event, on.as_ref(), Some("../x")),
                "unsafe id: {event}"
            );
        }
        assert!(!uses_adaptive(
            HookEvent::SessionEnd,
            on.as_ref(),
            Some("s1")
        ));
    }

    #[test]
    fn local_recording_events_skip_the_memory_pipeline() {
        assert!(records_locally(HookEvent::PostToolBatch));
        assert!(records_locally(HookEvent::SubagentTask));
        assert!(
            !records_locally(HookEvent::PreToolUse),
            "the general PreToolUse hook must not write the ledger"
        );
        assert!(!records_locally(HookEvent::TurnStart));
    }

    #[test]
    fn scope_recall_actions_is_turn_start_without_the_final_recall() {
        let tags = tag_recall_queries(&["a".to_string()]).unwrap();
        let turn = dispatch(
            HookEvent::TurnStart,
            &tags,
            &[],
            &BTreeMap::new(),
            Some(&WakeUpArgs::default()),
        );
        let scope = scope_recall_actions(&tags, &[], &BTreeMap::new());
        assert_eq!(turn.last(), Some(&Action::Recall(String::new())));
        assert_eq!(scope, turn[..turn.len() - 1]);
    }

    #[test]
    fn adaptive_recall_events_round_trip_through_their_names() {
        for (name, event) in [
            ("post_tool_batch", HookEvent::PostToolBatch),
            ("post_tool_use_failure", HookEvent::PostToolUseFailure),
            ("subagent_start", HookEvent::SubagentStart),
            ("subagent_task", HookEvent::SubagentTask),
            ("post_model_switch", HookEvent::PostModelSwitch),
        ] {
            assert_eq!(name.parse::<HookEvent>().unwrap(), event);
            assert_eq!(event.to_string(), name);
            assert!(
                dispatch(
                    event,
                    &[],
                    &[],
                    &BTreeMap::new(),
                    Some(&WakeUpArgs::default())
                )
                .is_empty()
            );
            assert_eq!(event_to_log_kind(event), None);
        }
    }

    #[test]
    fn all_hook_events_covers_every_variant() {
        for name in ALL_HOOK_EVENTS {
            let parsed = HookEvent::from_str(name).unwrap();
            assert_eq!(&parsed.to_string(), name, "{name} does not round-trip");
        }
        // `HookEvent` derives no variant count, so this guards the list
        // against a variant added to the enum but not to `from_str`.
        assert_eq!(ALL_HOOK_EVENTS.len(), 16);
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
                if event == HookEvent::SessionStart && adapter.name() == "claude-code" {
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
        let text = resolve_stop_reminder(
            state_dir.path(),
            Some("s1"),
            &with_critique,
            &serde_json::Value::Null,
        );
        assert!(text.contains("tests"), "the critique is present: {text:?}");
        assert!(
            !text.starts_with('\n'),
            "no separator before the first part: {text:?}"
        );

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
        let plain = resolve_stop_reminder(
            state_dir.path(),
            Some("s1"),
            &without,
            &serde_json::Value::Null,
        );
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
    /// Start a session that the conversation `conversation` owns, as `llmenv task session start`
    /// does inside an engine.
    fn start_owned_session(state_dir: &std::path::Path, project: &str, conversation: &str) {
        let owner = crate::task::session::EngineIdentity::from_session_id(Some(conversation));
        let request = crate::task::session::StartRequest {
            name: None,
            description: None,
            project,
            owner: &owner,
            resume: &crate::task::resume::ResumeContext::default(),
        };
        crate::task::session::start_session_as(
            state_dir,
            &request,
            crate::task::session::StartDecision::Auto,
        )
        .expect("test");
    }

    #[test]
    fn stop_reminder_separates_tracker_text_from_the_critique() {
        let state_dir = tempfile::tempdir().expect("test");
        // The reminder only lists `wip` tasks belonging to the *current*
        // project, resolved from cwd — a fixture project string would be
        // filtered straight back out.
        let project = crate::task::project::current_tag().expect("test");
        start_owned_session(state_dir.path(), &project, "s1");
        let task = crate::task::add_task(
            state_dir.path(),
            "finish the parser",
            crate::task::ParentSpec::Detached,
            crate::task::SessionChoice::Resolve(&crate::task::session::EngineIdentity::default()),
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
        let text = resolve_stop_reminder(
            state_dir.path(),
            Some("s1"),
            &config,
            &serde_json::Value::Null,
        );
        let tracker_only = crate::task::stop_hook_reminder(state_dir.path(), None);
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

    // #2456: a turn that ends with a question adds the waiting reminder after the tracker text,
    // and adds it alone when the tracker said nothing.
    #[test]
    fn stop_reminder_appends_the_waiting_reminder_after_a_question() {
        let state_dir = tempfile::tempdir().expect("test");
        let project = crate::task::project::current_tag().expect("test");
        let config = crate::config::Config {
            features: Some(crate::config::Features {
                task_tracker: Some(crate::config::TaskTracker {
                    enabled: true,
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
        let question = serde_json::json!({ "last_assistant_message": "Which one?" });
        // No task: nothing to say, and no stray separator.
        let empty = resolve_stop_reminder(state_dir.path(), Some("s1"), &config, &question);
        assert_eq!(empty, "");
        start_owned_session(state_dir.path(), &project, "s1");
        let task = crate::task::add_task(
            state_dir.path(),
            "finish the parser",
            crate::task::ParentSpec::Detached,
            crate::task::SessionChoice::Resolve(&crate::task::session::EngineIdentity::default()),
            &project,
        )
        .expect("test");
        crate::task::start_task(state_dir.path(), &task.slug, false).expect("test");
        let text = resolve_stop_reminder(state_dir.path(), Some("s1"), &config, &question);
        let tracker_only = crate::task::stop_hook_reminder(state_dir.path(), None);
        assert!(
            !tracker_only.is_empty(),
            "the fixture must produce tracker text"
        );
        let expected_tail = format!(
            "\n\nllmenv task tracker: you asked the user a question while '{}'",
            task.slug
        );
        assert!(text.starts_with(tracker_only.as_str()), "{text:?}");
        assert!(
            text[tracker_only.len()..].starts_with(&expected_tail),
            "{text:?}"
        );
        let statement = serde_json::json!({ "last_assistant_message": "Done." });
        let quiet = resolve_stop_reminder(state_dir.path(), Some("s1"), &config, &statement);
        assert_eq!(quiet, tracker_only);
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
        // SessionStart + "claude-code" triggers the check; other engines do not.
        assert!(
            HookEvent::SessionStart == HookEvent::SessionStart && "claude-code" == "claude-code"
        );
        assert!(
            !(HookEvent::SessionStart == HookEvent::SessionStart && "claude_code" == "claude-code")
        );
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

        let only_config = hook_marker(HookTiming::new(t0, t_config));
        assert_eq!(
            keys_of(&only_config),
            BTreeSet::from([
                "v".to_string(),
                "op".to_string(),
                "config_load_us".to_string()
            ]),
            "an early return before scope eval must report v and config_load_us"
        );

        let t_scope = t_config + std::time::Duration::from_micros(20);
        let through_scope = hook_marker(HookTiming {
            t_scope: Some(t_scope),
            ..HookTiming::new(t0, t_config)
        });
        assert_eq!(
            keys_of(&through_scope),
            BTreeSet::from([
                "v".to_string(),
                "op".to_string(),
                "config_load_us".to_string(),
                "scope_eval_us".to_string(),
            ]),
            "an early return after scope eval must add scope_eval_us"
        );

        let t_chunk = t_scope + std::time::Duration::from_micros(30);
        let t_end = t_chunk + std::time::Duration::from_micros(40);
        let full = hook_marker(HookTiming {
            t_scope: Some(t_scope),
            t_chunk: Some(t_chunk),
            t_end: Some(t_end),
            ..HookTiming::new(t0, t_config)
        });
        assert_eq!(
            keys_of(&full),
            BTreeSet::from([
                "v".to_string(),
                "op".to_string(),
                "config_load_us".to_string(),
                "scope_eval_us".to_string(),
                "prep_us".to_string(),
                "mcp_us".to_string(),
            ]),
            "the full success path must report v and all four phases"
        );
        assert_eq!(full["v"], 1);
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

            let v = hook_marker(HookTiming {
            t_scope: Some(t_scope),
            t_chunk: Some(t_chunk),
            t_end: Some(t_end),
            ..HookTiming::new(t0, t_config)
        });
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
            always_load: None,
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
            adaptive_recall: true,
            global_topics: crate::config::Memory::default().global_topics,
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
                settings: default_memory_hook(),
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
                settings: default_memory_hook(),
            },
            "a key mismatch must fall back to a correct live merge, never the stale cache"
        );
    }

    // Static MCPs resolve ahead of the memory backend, so an unnamed match
    // would hand the hooks another server's URL.
    #[test]
    fn memory_url_skips_other_remote_mcps() {
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
                "      when: [mytag]\n",
                "host:\n",
                "  still:\n",
                "    addr: still.local\n",
            ),
        )
        .expect("test");
        let cache_dir = tempfile::tempdir().expect("test");
        let other: crate::config::McpServer = serde_json::from_value(serde_json::json!({
            "name": "aaa-remote", "when": ["mytag"], "type": "http",
            "url": "http://other.invalid/mcp"
        }))
        .expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            mcp: vec![other],
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
        let url = memory_url(&config, config_root.path(), &active).expect("test");
        assert_eq!(
            url,
            MemoryEndpoint::Active {
                url: "http://still.local:7878/mcp".into(),
                settings: default_memory_hook(),
            }
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
        redirect_stderr_to_bounded_log(&mut cmd, &log, true, "test");

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log)
            .expect("a detached child's stderr must reach a file, not /dev/null");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }

    // Pins the shared log name: the three detached children, the docs, and any
    // operator told where to look must all agree on one path.
    #[cfg(unix)]
    #[test]
    fn a_detached_child_stderr_reaches_the_log_it_is_given() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let log = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo child-stderr-marker >&2"]);
        redirect_stderr_to_detached_log(&mut cmd, Ok(log.clone()));
        anyhow::ensure!(cmd.status()?.success(), "the child must run");
        let text = std::fs::read_to_string(&log)?;
        anyhow::ensure!(
            text.contains("child-stderr-marker"),
            "the child's stderr must reach the log: {text}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_unresolvable_log_path_still_runs_the_detached_child() -> anyhow::Result<()> {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo discarded >&2"]);
        redirect_stderr_to_detached_log(&mut cmd, Err(anyhow::anyhow!("no state dir")));
        anyhow::ensure!(cmd.status()?.success(), "the child must run without a log");
        Ok(())
    }

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
                    wakeup_max_tokens: Some(750),
                    global_topics: vec![],
                    ..crate::config::Memory::default()
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
        assert_eq!(
            endpoint.settings().and_then(|s| s.wakeup_max_tokens),
            Some(750)
        );
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
                    global_topics: vec![],
                    ..crate::config::Memory::default()
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
                    global_topics: vec![],
                    ..crate::config::Memory::default()
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
                "the deny must lead the string; got {} bytes",
                text.len()
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
            "tool_input": { "command": "ls && cd /tmp" },
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
            "tool_input": { "command": "ls && cd /tmp" },
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
            "tool_input": { "command": "ls && cd /tmp" },
        });
        let text = resolve_pre_tool_text(&payload, Some("s2"), &config, false, state_dir.path())
            .expect("test");
        assert!(
            text.contains("cd"),
            "cd_guard text must be present; got {} bytes",
            text.len()
        );
        assert!(
            text.contains("identical input"),
            "repeat_detect text must also be present; got {} bytes",
            text.len()
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
            "deny must name the created task; got {} bytes",
            text.len()
        );
    }

    #[test]
    fn index_repository_command_sets_args_and_no_scoping_env() {
        let cm = crate::config::CodebaseMemory {
            allowed_roots: vec![],
            mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        let cmd = index_command(
            std::path::Path::new("codebase-memory-mcp"),
            std::path::Path::new("/repos/proj"),
            cm.index_path.as_deref(),
            cm.mem_budget_mb,
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
        assert!(
            !envs.contains_key("CBM_ALLOWED_ROOT"),
            "llmenv must not set CBM_ALLOWED_ROOT (#1495) — \
                 codebase-memory-mcp applies its own default scoping"
        );
        assert!(
            !envs.contains_key("CBM_CACHE_DIR"),
            "llmenv must not set CBM_CACHE_DIR by default (#1493) — \
                 codebase-memory-mcp falls back to its own default cache location"
        );
    }

    #[test]
    fn index_repository_command_index_path_override_wins() {
        let cm = crate::config::CodebaseMemory {
            allowed_roots: vec![],
            mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: Some("/custom/path".to_string()),
            mcp_permissions: None,
        };
        let cmd = index_command(
            std::path::Path::new("codebase-memory-mcp"),
            std::path::Path::new("/repos/proj"),
            cm.index_path.as_deref(),
            cm.mem_budget_mb,
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
    fn index_checkpoint_is_not_opened_when_the_executable_is_unknown() {
        let state = tempfile::tempdir().expect("test");
        let inputs = detached_cbm::IndexInputs {
            project_root: "/repos/proj".to_string(),
            index_path: None,
            mem_budget_mb: None,
            result_path: None,
        };
        // Without the executable no wrapper can settle the checkpoint, so none is written.
        assert_eq!(begin_index_checkpoint(state.path(), &inputs, false), None);
        assert_eq!(
            std::fs::read_dir(state.path()).expect("test").count(),
            0,
            "no checkpoint file may be written"
        );
    }

    #[test]
    fn index_checkpoint_is_opened_when_the_executable_is_known() {
        let state = tempfile::tempdir().expect("test");
        let inputs = detached_cbm::IndexInputs {
            project_root: "/repos/proj".to_string(),
            index_path: None,
            mem_budget_mb: None,
            result_path: None,
        };
        let path = begin_index_checkpoint(state.path(), &inputs, true)
            .expect("a checkpoint opens when the executable is known");
        assert!(path.exists(), "the checkpoint file must be written");
    }

    #[test]
    fn index_skipped_notice_names_the_skip_and_its_cause() {
        let notice = index_skipped_notice("cannot expand index_path: HOME is not set");
        assert!(
            notice.contains("index skipped"),
            "unexpected notice: {notice}"
        );
        assert!(
            notice.contains("HOME is not set"),
            "unexpected notice: {notice}"
        );
        assert!(notice.ends_with('\n'), "a notice line ends with a newline");
    }

    #[test]
    fn expanded_index_path_removes_a_leading_tilde() {
        let cm = crate::config::CodebaseMemory {
            allowed_roots: vec![],
            mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: Some("~/cbm".to_string()),
            mcp_permissions: None,
        };
        let expanded = expanded_index_path(&cm).ok().flatten();
        assert!(
            expanded
                .as_deref()
                .is_some_and(|p| !p.starts_with('~') && p.ends_with("/cbm")),
            "a literal tilde must not reach the indexer: {expanded:?}"
        );
    }

    #[test]
    fn expanded_index_path_passes_an_absolute_path_and_none_through() {
        let mut cm = crate::config::CodebaseMemory {
            allowed_roots: vec![],
            mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: Some("/custom/path".to_string()),
            mcp_permissions: None,
        };
        assert_eq!(
            expanded_index_path(&cm).ok().flatten().as_deref(),
            Some("/custom/path")
        );
        cm.index_path = None;
        assert_eq!(expanded_index_path(&cm).unwrap(), None);
    }

    #[test]
    fn codebase_memory_cache_dir_defaults_under_state_dir() {
        let cm = crate::config::CodebaseMemory {
            allowed_roots: vec![],
            mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        assert_eq!(
            codebase_memory_cache_dir(&cm, std::path::Path::new("/state")).ok(),
            Some(std::path::PathBuf::from("/state/codebase-memory"))
        );
    }

    #[test]
    fn codebase_memory_cache_dir_honors_index_path_override() {
        let cm = crate::config::CodebaseMemory {
            allowed_roots: vec![],
            mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: Some("/custom/path".to_string()),
            mcp_permissions: None,
        };
        assert_eq!(
            codebase_memory_cache_dir(&cm, std::path::Path::new("/state")).ok(),
            Some(std::path::PathBuf::from("/custom/path"))
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
    fn the_index_command_and_the_mcp_server_agree_on_the_memory_budget() {
        let env_of = |cmd: &std::process::Command, key: &str| {
            cmd.get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(key))
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let with = crate::config::CodebaseMemory {
            when: vec!["p".into()],
            mem_budget_mb: Some(8192),
            ..Default::default()
        };
        let cmd = index_command(
            std::path::Path::new("codebase-memory-mcp"),
            std::path::Path::new("/r"),
            with.index_path.as_deref(),
            with.mem_budget_mb,
        );
        assert_eq!(env_of(&cmd, "CBM_MEM_BUDGET_MB").as_deref(), Some("8192"));
        let without = crate::config::CodebaseMemory {
            when: vec!["p".into()],
            ..Default::default()
        };
        let cmd = index_command(
            std::path::Path::new("codebase-memory-mcp"),
            std::path::Path::new("/r"),
            without.index_path.as_deref(),
            without.mem_budget_mb,
        );
        assert_eq!(env_of(&cmd, "CBM_MEM_BUDGET_MB"), None);
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
            let cm = crate::config::CodebaseMemory { allowed_roots: vec![], mem_budget_mb: None,
                when: vec!["proj".to_string()],
                index_path: None,
                mcp_permissions: None,
            };
            let project_root = std::path::PathBuf::from(&path_str);
            let cmd = index_command(
                std::path::Path::new("codebase-memory-mcp"),
                &project_root,
                cm.index_path.as_deref(),
                cm.mem_budget_mb,
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

        // #1493/#1495: index_command must never set
        // CBM_ALLOWED_ROOT, and must never set CBM_CACHE_DIR when
        // index_path is unset — mirrors resolve_codebase_memory's contract
        // (src/mcp/resolve.rs), for arbitrary project_root content.
        #[test]
        fn index_repository_command_no_scoping_env_when_index_path_none(
            path_str in "[\\PC]{0,60}"
        ) {
            let cm = crate::config::CodebaseMemory { allowed_roots: vec![], mem_budget_mb: None,
                when: vec!["proj".to_string()],
                index_path: None,
                mcp_permissions: None,
            };
            let project_root = std::path::PathBuf::from(&path_str);
            let cmd = index_command(
                std::path::Path::new("codebase-memory-mcp"),
                &project_root,
                cm.index_path.as_deref(),
                cm.mem_budget_mb,
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
            proptest::prop_assert!(!envs.contains_key("CBM_ALLOWED_ROOT"));
            proptest::prop_assert!(!envs.contains_key("CBM_CACHE_DIR"));
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
            assert_eq!(
                dispatch(ev, &[], &[], &BTreeMap::new(), Some(&WakeUpArgs::default())),
                Vec::<Action>::new()
            );
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
            dispatch(
                HookEvent::SessionStart,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&WakeUpArgs::default())
            ),
            vec![Action::WakeUp(WakeUpArgs::default())]
        );
        assert_eq!(
            dispatch(
                HookEvent::TurnStart,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&WakeUpArgs::default())
            ),
            vec![Action::Recall(String::new())]
        );
        assert_eq!(
            dispatch(
                HookEvent::SessionEnd,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&WakeUpArgs::default())
            ),
            vec![Action::Store]
        );
        assert_eq!(
            dispatch(
                HookEvent::PostSession,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&WakeUpArgs::default())
            ),
            vec![],
            "PostSession defers to consolidation module, no dispatch actions"
        );
    }

    // #2142: a continued session (resume, fork) gets no wake-up call.
    #[test]
    fn dispatch_skips_the_wake_up_without_wake_args() {
        assert_eq!(
            dispatch(HookEvent::SessionStart, &[], &[], &BTreeMap::new(), None),
            vec![]
        );
    }

    #[test]
    fn continues_session_only_for_resume_and_fork() {
        for (payload, want) in [
            (json!({"source": "resume"}), true),
            (json!({"source": "fork"}), true),
            (json!({"source": "startup"}), false),
            (json!({"source": "clear"}), false),
            (json!({"source": "compact"}), false),
            (json!({"source": 7}), false),
            (json!({}), false),
        ] {
            assert_eq!(continues_session(&payload), want, "{payload}");
        }
    }

    #[test]
    fn dispatch_threads_wakeup_max_tokens_into_session_start_only() {
        let wake_750 = WakeUpArgs {
            max_tokens: Some(750),
            project: Some("llmenv".into()),
        };
        assert_eq!(
            dispatch(
                HookEvent::SessionStart,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&wake_750)
            ),
            vec![Action::WakeUp(wake_750.clone())]
        );
        // WakeUp only fires on SessionStart; TurnStart takes just the project (#2253).
        assert_eq!(
            dispatch(
                HookEvent::TurnStart,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&wake_750)
            ),
            vec![Action::Recall("llmenv".into())]
        );
    }

    #[test]
    fn chunk_unchanged_true_when_prev_matches_current() {
        assert!(chunk_unchanged(Some("same"), "same"));
    }

    #[test]
    fn chunk_unchanged_false_when_prev_differs_from_current() {
        assert!(!chunk_unchanged(Some("old"), "new"));
    }

    #[test]
    fn chunk_unchanged_false_when_no_prior_snapshot() {
        // No snapshot yet (first SessionEnd, or a read failure) is always
        // "changed" — never skip the very first store.
        assert!(!chunk_unchanged(None, "anything"));
    }

    #[test]
    fn store_content_for_event_session_end_uses_storage_chunk() {
        assert_eq!(
            store_content_for_event(HookEvent::SessionEnd, "full", "minimal"),
            "minimal"
        );
    }

    #[test]
    fn store_content_for_event_other_events_use_full_chunk() {
        assert_eq!(
            store_content_for_event(HookEvent::TurnStart, "full", "minimal"),
            "full"
        );
        assert_eq!(
            store_content_for_event(HookEvent::SessionStart, "full", "minimal"),
            "full"
        );
    }

    #[tokio::test]
    async fn run_memory_actions_joins_deduplicated_recall_records() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": [{ "type": "text", "text": "[t] one\n[t] two\nNo memories found." }] }
        });
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("URL");

        let text = run_memory_actions(
            &client,
            vec![Action::Recall(String::new()), Action::Recall(String::new())],
            "q",
            "chunk",
        )
        .await;

        assert_eq!(text.text(), "[t] one\n\n[t] two");
    }

    #[test]
    fn turn_start_expands_one_recall_tag_per_active_tag() {
        let tags = vec!["rust".to_string(), "work-vpn".to_string()];
        let queries = tag_recall_queries(&tags).expect("valid tags");
        let actions = dispatch(
            HookEvent::TurnStart,
            &queries,
            &[],
            &BTreeMap::new(),
            Some(&WakeUpArgs::default()),
        );
        assert_eq!(
            actions,
            vec![
                Action::RecallTag(TagRecallQuery {
                    tag: "rust".to_string(),
                    keyword: "llmenv-tag:rust".to_string(),
                }),
                Action::RecallTag(TagRecallQuery {
                    tag: "work-vpn".to_string(),
                    keyword: "llmenv-tag:work-vpn".to_string(),
                }),
                Action::Recall(String::new()),
            ],
            "TurnStart must run one tag recall per active tag, then the project recall"
        );
    }

    #[test]
    fn turn_start_expands_one_recall_bundle_per_active_bundle() {
        let bundles = vec!["base".to_string(), "rust-defaults".to_string()];
        let queries = bundle_recall_queries(&bundles).expect("valid bundles");
        let actions = dispatch(
            HookEvent::TurnStart,
            &[],
            &queries,
            &BTreeMap::new(),
            Some(&WakeUpArgs::default()),
        );
        assert_eq!(
            actions,
            vec![
                Action::RecallBundle(BundleRecallQuery {
                    bundle: "base".to_string(),
                    keyword: "llmenv-bundle:base".to_string(),
                }),
                Action::RecallBundle(BundleRecallQuery {
                    bundle: "rust-defaults".to_string(),
                    keyword: "llmenv-bundle:rust-defaults".to_string(),
                }),
                Action::Recall(String::new()),
            ],
            "TurnStart must emit one bundle recall per active bundle"
        );
    }

    #[test]
    fn turn_start_interleaves_tag_and_bundle_recalls() {
        let tag_qs = tag_recall_queries(&["rust".to_string()]).expect("valid");
        let bundle_qs = bundle_recall_queries(&["base".to_string()]).expect("valid");
        let ranks = BTreeMap::from([("rust".to_string(), 1)]);
        let actions = dispatch(
            HookEvent::TurnStart,
            &tag_qs,
            &bundle_qs,
            &ranks,
            Some(&WakeUpArgs::default()),
        );
        // Order: rank-1 tag recalls, then bundle recalls, then project recall.
        assert!(matches!(actions[0], Action::RecallTag(_)));
        assert!(matches!(actions[1], Action::RecallBundle(_)));
        assert_eq!(actions[2], Action::Recall(String::new()));
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
        let ranks = BTreeMap::from([("foo".to_string(), 1)]);
        let actions = dispatch(
            HookEvent::TurnStart,
            &tag_qs,
            &bundle_qs,
            &ranks,
            Some(&WakeUpArgs::default()),
        );
        assert_eq!(actions.len(), 3);
        match &actions[0] {
            Action::RecallTag(q) => assert_eq!(q.keyword, "llmenv-tag:foo"),
            other => panic!("expected RecallTag, got {other:?}"),
        }
        match &actions[1] {
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
        // With no ranks, dispatch(TurnStart) always produces
        // [M×RecallBundle, N×RecallTag, Recall] regardless of N and M.
        #[test]
        fn prop_dispatch_turn_start_ordering(
            tags in proptest::collection::vec(valid_name(), 0..8),
            bundles in proptest::collection::vec(valid_name(), 0..8),
        ) {
            let tag_qs = tag_recall_queries(&tags).expect("valid tags");
            let bundle_qs = bundle_recall_queries(&bundles).expect("valid bundles");
            let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs, &BTreeMap::new(), Some(&WakeUpArgs::default()));

            prop_assert_eq!(actions.len(), 1 + tags.len() + bundles.len());
            for a in &actions[..bundles.len()] {
                prop_assert!(
                    matches!(a, Action::RecallBundle(_)),
                    "expected RecallBundle, got {a:?}"
                );
            }
            for a in &actions[bundles.len()..bundles.len() + tags.len()] {
                prop_assert!(matches!(a, Action::RecallTag(_)), "expected RecallTag, got {a:?}");
            }
            prop_assert!(matches!(actions.last(), Some(Action::Recall(_))));
        }
    }

    #[test]
    fn turn_start_orders_tags_by_specificity_with_bundles_after_rank_one() {
        let tags: Vec<String> = [
            "aaa-host",
            "bbb-project",
            "ccc-user",
            "ddd-project",
            "eee-loose",
        ]
        .map(String::from)
        .to_vec();
        let ranks = BTreeMap::from([
            ("aaa-host".to_string(), 5),
            ("bbb-project".to_string(), 1),
            ("ccc-user".to_string(), 4),
            ("ddd-project".to_string(), 1),
        ]);
        let tag_qs = tag_recall_queries(&tags).expect("valid tags");
        let bundle_qs = bundle_recall_queries(&["base".to_string()]).expect("valid bundle");
        let order: Vec<String> = dispatch(
            HookEvent::TurnStart,
            &tag_qs,
            &bundle_qs,
            &ranks,
            Some(&WakeUpArgs::default()),
        )
        .iter()
        .map(|a| match a {
            Action::RecallTag(q) => q.tag.clone(),
            Action::RecallBundle(q) => format!("bundle:{}", q.bundle),
            other => format!("{other:?}"),
        })
        .collect();
        assert_eq!(
            order,
            [
                "bbb-project",
                "ddd-project",
                "bundle:base",
                "ccc-user",
                "aaa-host",
                "eee-loose",
                "Recall(\"\")",
            ]
        );
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
            global_topics: vec![],
            ..crate::config::Memory::default()
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

    fn memory_settings(default_type: Option<llmenv_config::MemoryType>) -> MemoryHookSettings {
        MemoryHookSettings {
            adaptive_recall: true,
            default_type,
            global_topics: vec![],
            ..MemoryHookSettings::default()
        }
    }

    #[test]
    fn apply_memory_defaults_idempotent_no_type() {
        let settings = memory_settings(None);
        let input = "## context\nno markers".to_string();
        let once = apply_memory_config_defaults(input, Some(&settings));
        let twice = apply_memory_config_defaults(once.clone(), Some(&settings));
        assert_eq!(once, twice, "applying defaults twice must be idempotent");
    }

    #[test]
    fn apply_memory_defaults_adds_type_marker_when_present() {
        let settings = memory_settings(Some(llmenv_config::MemoryType::Semantic));
        let input = "## context".to_string();
        let out = apply_memory_config_defaults(input, Some(&settings));
        assert!(
            out.contains("<!-- llmenv-type: semantic -->"),
            "should add semantic type marker: {out}"
        );
    }

    #[test]
    fn apply_memory_defaults_adds_importance_marker_once() {
        let settings = MemoryHookSettings {
            default_importance: Some(llmenv_config::ImportanceLevel::High),
            ..memory_settings(None)
        };
        let out = apply_memory_config_defaults("## context".to_string(), Some(&settings));
        assert!(out.contains("<!-- llmenv-importance: high -->"), "{out}");
        let kept = apply_memory_config_defaults(
            "## context\n<!-- llmenv-importance: low -->".to_string(),
            Some(&settings),
        );
        assert!(!kept.contains("high"), "an existing marker wins: {kept}");
        assert!(apply_memory_config_defaults("x".to_string(), None) == "x");
    }

    #[test]
    fn apply_memory_defaults_skips_existing_marker() {
        let settings = memory_settings(Some(llmenv_config::MemoryType::Semantic));
        let input = "## context\n<!-- llmenv-type: episodic -->".to_string();
        let out = apply_memory_config_defaults(input, Some(&settings));
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
        // Reap: production deliberately never waits (the child is
        // process-group-detached), but this test process is still its OS
        // parent — leaving it un-waited leaks a zombie for the rest of the
        // cargo-test run (#1095).
        assert!(child.is_some(), "the store child must start");
        if let Some(mut child) = child {
            reap_test_child(&mut child, std::time::Duration::from_secs(5));
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
    async fn ensure_transcript_session_starts_no_second_transcript_while_one_is_pending() {
        // A start whose link was not recorded leaves its pending marker. A later event must not
        // start another transcript, which would be a second orphan (#2834).
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        let seen = state::Link::Missing;
        let now = super::session_state::unix_now();
        assert!(state::claim_pending_at(&state_path, "claude-p", &seen, now).unwrap());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-new")))
            .expect(0)
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
            "claude-p",
            &ctx(),
            Some(&state_path),
            true,
        )
        .await;

        assert_eq!(id, None);
        server.verify().await;
    }

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
            state::lookup_session_at(&state_path, "claude-1")
                .unwrap()
                .as_deref(),
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
    async fn ensure_transcript_session_notices_a_map_it_cannot_read() {
        // #2658: a map that cannot be read must not start a transcript that cannot be linked.
        // The agent is told, because stderr from a hook never reaches it (#2818).
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        std::fs::write(&state_path, "{not json").unwrap();
        let cfg = SessionLog {
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                retention_days: None,
            }),
            ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
        };

        let id =
            ensure_transcript_session(&cfg, None, "claude-6", &ctx(), Some(&state_path), false)
                .await;

        assert!(id.is_none());
        let notice = crate::agent_notice::current().unwrap_or_default();
        assert!(
            notice.contains("no ICM transcript started for this session"),
            "{notice}"
        );
        assert!(notice.contains("Fix: run `llmenv doctor`"), "{notice}");
    }

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
            // The text icm-mcp sends for an unknown session: `session <id> not found`.
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc":"2.0","id":1,
                "result":{"isError":true,"content":[
                    {"type":"text","text":"session icm-stale not found"}
                ]}
            })))
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
            state::lookup_session_at(&state_path, "claude-4")
                .unwrap()
                .as_deref(),
            Some("icm-fresh"),
            "the correlation map must be updated to the fresh id"
        );
    }

    #[tokio::test]
    async fn ensure_transcript_session_keeps_link_when_icm_is_unreachable() {
        // #2667: a transient ICM outage must not orphan a valid transcript.
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        state::record_session_at(&state_path, "claude-5", "icm-live").unwrap();
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
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                crate::session_log::transcript::START_TOOL,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-new")))
            .expect(0)
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
            "claude-5",
            &ctx(),
            Some(&state_path),
            true,
        )
        .await;

        assert_eq!(id.as_deref(), Some("icm-live"));
        assert_eq!(
            state::lookup_session_at(&state_path, "claude-5")
                .unwrap()
                .as_deref(),
            Some("icm-live"),
            "the link must survive an unreachable ICM"
        );
    }

    #[tokio::test]
    async fn ensure_transcript_session_starts_nothing_when_the_map_is_unwritable() {
        // #2667: a transcript that cannot be linked is an orphan, so none is started.
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("transcript-sessions.json");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                crate::session_log::transcript::START_TOOL,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-new")))
            .expect(0)
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
        // A directory where the lock file belongs makes the map unwritable. The lock step
        // resets the state dir mode, so a read-only dir would not stop the write.
        std::fs::create_dir(state_dir.path().join("transcript-sessions.lock")).unwrap();

        let id = ensure_transcript_session(
            &cfg,
            Some(&client),
            "claude-6",
            &ctx(),
            Some(&state_path),
            true,
        )
        .await;

        assert_eq!(id, None);
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

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod adaptive_routing_tests {
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn run(
        event: HookEvent,
        payload: &serde_json::Value,
        adaptive_recall: bool,
    ) -> tempfile::TempDir {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {"content": [{"type": "text", "text": ""}]}
            })))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        run_event_memory(MemoryCall {
            event,
            client: &client,
            settings: Some(&MemoryHookSettings {
                adaptive_recall,
                global_topics: vec![],
                ..MemoryHookSettings::default()
            }),
            session_id: Some("s1"),
            state_dir: Some(state_dir.path().to_path_buf()),
            payload,
            actions: Vec::new(),
            scope: Vec::new(),
            wake: WakeUpArgs::default(),
            query: "",
            store_content: "",
        })
        .await
        .unwrap();
        state_dir
    }

    fn wrote_ledger(state_dir: &tempfile::TempDir) -> bool {
        state_dir.path().join("recall_session/s1.json").is_file()
    }

    fn adaptive_events() -> [(HookEvent, serde_json::Value); 4] {
        [
            (HookEvent::SessionStart, json!({"source": "startup"})),
            (HookEvent::TurnStart, json!({"prompt": "hi"})),
            (
                HookEvent::PostToolUseFailure,
                json!({"tool_name": "Bash", "error": "x"}),
            ),
            (
                HookEvent::SubagentStart,
                json!({"agent_id": "a1", "agent_type": "Explore"}),
            ),
        ]
    }

    #[tokio::test]
    async fn adaptive_events_take_the_ledger_flow() {
        for (event, payload) in adaptive_events() {
            let dir = run(event, &payload, true).await;
            assert!(wrote_ledger(&dir), "{event:?} must use the adaptive flow");
        }
    }

    #[tokio::test]
    async fn adaptive_recall_off_keeps_the_stateless_flow() {
        for (event, payload) in adaptive_events() {
            let dir = run(event, &payload, false).await;
            assert!(!wrote_ledger(&dir), "{event:?} must stay stateless");
        }
    }

    fn git_repo_with_origin() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["remote", "add", "origin", "git@github.com:me/wake-repo.git"],
        ] {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        dir
    }

    #[test]
    fn project_lookup_runs_for_session_start_and_stateless_turn_start_only() {
        let repo = git_repo_with_origin();
        let cwd = repo.path().to_str().unwrap();
        let payload = json!({"cwd": cwd});
        let adaptive = Some(MemoryHookSettings {
            adaptive_recall: true,
            global_topics: vec![],
            ..MemoryHookSettings::default()
        });
        let start = wake_args(
            HookEvent::SessionStart,
            adaptive.as_ref(),
            &payload,
            "/",
            Some("s1"),
        )
        .ok()
        .and_then(|w| w.project);
        assert_eq!(start.as_deref(), Some("wake-repo"));
        // #2253: the stateless TurnStart recall is scoped to the session project.
        let stateless = wake_args(HookEvent::TurnStart, None, &payload, "/", Some("s1"))
            .ok()
            .and_then(|w| w.project);
        assert_eq!(stateless.as_deref(), Some("wake-repo"));
        let adaptive_turn = wake_args(
            HookEvent::TurnStart,
            adaptive.as_ref(),
            &payload,
            "/",
            Some("s1"),
        )
        .ok()
        .and_then(|w| w.project);
        assert_eq!(adaptive_turn, None, "the ledger holds the project");
        // Adaptive is on, but no session id means the stateless path runs. It must still scope.
        let no_session = wake_args(HookEvent::TurnStart, adaptive.as_ref(), &payload, "/", None)
            .ok()
            .and_then(|w| w.project);
        assert_eq!(
            no_session.as_deref(),
            Some("wake-repo"),
            "no ledger, so no project otherwise"
        );
        let other = wake_args(HookEvent::PostToolUse, None, &payload, "/", None)
            .ok()
            .and_then(|w| w.project);
        assert_eq!(other, None);
    }

    #[test]
    fn wake_args_refuses_to_guess_a_project_when_git_cannot_start() {
        let payload = serde_json::json!({ "cwd": "/no/such/dir/for/llmenv-hook" });
        let err = wake_args(HookEvent::SessionStart, None, &payload, "/", None)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("cannot run git") && err.contains("/no/such/dir/for/llmenv-hook"),
            "the agent must see the git failure and the directory: {err}"
        );
    }

    #[test]
    fn stateless_turn_start_recall_carries_the_session_project() {
        let wake = WakeUpArgs {
            max_tokens: None,
            project: Some("llmenv".into()),
        };
        let turn = dispatch(
            HookEvent::TurnStart,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&wake),
        );
        assert_eq!(turn, vec![Action::Recall("llmenv".into())]);
    }

    #[test]
    fn hook_cwd_prefers_the_payload_then_the_fallback() {
        assert_eq!(
            hook_cwd(&json!({"cwd": "/from/payload"}), "/fallback"),
            std::path::PathBuf::from("/from/payload")
        );
        assert_eq!(
            hook_cwd(&json!({}), "/fallback"),
            std::path::PathBuf::from("/fallback")
        );
    }
}
