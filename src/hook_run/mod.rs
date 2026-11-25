//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

mod name_check;
use name_check::{validate_bundle, validate_tag};
pub(crate) mod action;
mod adaptive;
pub(crate) mod agent_config;
mod bundle_names;
mod bundle_notice;
mod child_stderr;
mod dedup_snapshot;
use child_stderr::redirect_stderr_to_bounded_log;
pub(crate) use child_stderr::{detached_child_log_path, redirect_stderr_to_detached_log};
use dedup_snapshot::{dedup_snapshot_path, dedup_write_target, read_dedup_snapshot};
pub(crate) mod cbm_index_guard;
pub(crate) mod cd_guard;
pub(crate) mod checkpoint;
pub(crate) mod detached_cbm;
pub(crate) mod detached_consolidation;
pub(crate) mod detached_store;
mod engine_rules;
mod event;
mod memory_call;
mod memory_endpoint;
mod memory_resolve;
mod memory_target;
use memory_call::MemoryCallContext;
use memory_endpoint::{apply_memory_config_defaults, resolve_memory_client};
#[cfg(test)]
use memory_resolve::classify_missing_memory;
pub(crate) use memory_resolve::{memory_url, merged_memory, suppressed_memory_bundles};
pub(crate) use memory_target::MemoryEndpoint;
use memory_target::default_memory_hook;
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
mod recall_breaker;
pub(crate) mod recall_health;
mod record_hash;
mod relevance;
pub(crate) mod repeat_detect;
pub(crate) mod respawn;
mod resume_cache;
mod text_join;
use text_join::{append_section, join_notes};
mod session_emit;
mod session_ledger;
mod session_notices;
pub(crate) mod session_state;
mod shell_split;
pub(crate) mod shell_tool;
pub(crate) mod slippage;
mod slippage_read;
pub(crate) mod stop_dedupe;
#[cfg(test)]
pub(crate) mod test_recall;
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
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod stop_reminder_tests;

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
        failure_recall: call.settings.is_none_or(|s| s.failure_recall),
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
    // Claude Code, opencode, and Crush (v0.101.0 hook docs) all send the event name (#3769).
    if let Some(notice) = event::missing_name_notice(stdin_json.as_ref(), event, &hook_event_name) {
        crate::agent_notice::record(notice);
    }
    let claude_session_id: Option<&str> =
        stdin_json.as_ref().and_then(|v| v["session_id"].as_str());
    // The version is informational. Failing the hook here would exit 1, which lets the tool call
    // run unchecked, so a value that is not UTF-8 is logged and read as unset.
    let claude_code_version = match crate::env_var::utf8_var("CLAUDE_CODE_VERSION") {
        Ok(version) => version.unwrap_or_default(),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: CLAUDE_CODE_VERSION is read as unset, so version checks such as the \
                 resume notice use defaults: {e:#}"
            ));
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
    // The resume fields are parsed before the notices are taken, because a wrong-type field
    // records a notice as it is read.
    let resume_message = resume_cache::session_resume_notice(&hook_event_name, payload);
    // A deny envelope replaces the whole output, so recalled memory is not sent (#3399).
    if result.as_ref().is_ok_and(deny_drops_memory) {
        crate::agent_notice::record_logged(
            "llmenv: recalled memory was not shown, because the tool call was denied.",
        );
    }
    // Read after `run_inner`, so every output path below carries it to the agent (#2658).
    let notice = crate::agent_notice::current_with_children();
    match result {
        Ok(text) => {
            // #318: deny envelope detected — write a proper deny JSON envelope
            // to stdout so the Claude Code engine blocks the tool call.
            if text.notes.starts_with("__DENY__:") {
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
                let out = resume_cache::with_resume_notice(out, resume_message.as_deref());
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
            crate::agent_notice::no_state_folder("read-once and repeat detection are skipped", &e);
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
    text.notes.starts_with("__DENY__:") && !text.memory.is_empty()
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
            crate::paths::state_dir().inspect_err(|e| {
                crate::agent_notice::no_state_folder("the recall ledger is not updated", e);
            }),
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
    if resets_read_state(event, stdin_payload) {
        session_state::reset_read_state_after_compaction(
            claude_session_id,
            crate::paths::state_dir,
        );
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
            (_, Err(e)) => {
                crate::agent_notice::no_state_folder("the model switch is not recorded", &e);
            }
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
        let resolved_client = resolve_memory_client(
            &config,
            config_dir,
            &active,
            MemoryCallContext {
                event,
                state_dir: memory_call::notice_state_dir().as_deref(),
                session_id: claude_session_id,
            },
            &MCP_CLIENT_CACHE,
        );
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
                        state_dir: adaptive_state_dir(crate::paths::state_dir),
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
                        crate::agent_notice::record_logged(format!(
                            "llmenv: the session-end duplicate record {} is not saved, so the \
                             next session end can store the chunk again: {e:#}. Fix: check that \
                             the state folder is writable.",
                            dedup_path.display()
                        ));
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
                        crate::agent_notice::no_state_folder("the task reminder is skipped", &e);
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
            session_emit::reap_at_session_start(cfg);
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

/// The state folder for adaptive recall. A failure is noticed, because recall then runs without
/// adaptive ranking and the agent must be told why.
fn adaptive_state_dir(
    resolve: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    resolve()
        .inspect_err(|e| {
            crate::agent_notice::no_state_folder("adaptive recall is off for this event", e);
        })
        .ok()
}

/// Whether a PostToolUse event may store fetched web content in ICM (#579).
fn stores_web_fetch(event: HookEvent) -> bool {
    event == HookEvent::PostToolUse
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::expect_fun_call)]
mod pipeline_recovery_tests;

#[cfg(test)]
mod write_hook_line_tests;

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
    if !wrapped {
        let cause = if exe.is_none() {
            "llmenv cannot find its own program"
        } else {
            "the job has no checkpoint"
        };
        crate::agent_notice::record_logged(format!(
            "llmenv: the code index job runs without its wrapper, because {cause}, so a failure \
             of the indexer is not reported. Fix: run `llmenv doctor` to check the state folder."
        ));
    }
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
        Err(e) => crate::agent_notice::record_logged(format!(
            "llmenv: memory consolidation is skipped, because the memory settings cannot be \
             read: {e:#}. Fix: run `llmenv validate`."
        )),
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
#[allow(clippy::unwrap_used)]
mod session_log_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod adaptive_routing_tests;
