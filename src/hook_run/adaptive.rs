//! Adaptive recall flows for the lifecycle hooks (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::agent_notice;
#[cfg(test)]
use crate::hook_run::HookEvent;
use crate::hook_run::action::{Action, RecallQuery, Reply};
use crate::hook_run::recall::{RECALL_BUDGET_BYTES, RecallBudget};
use crate::hook_run::relevance::{self, TurnSignals};
use crate::hook_run::session_ledger::{Ledger, LedgerStore, LedgerTiming, MAIN_AGENT, record_hash};
use crate::hook_run::session_state::unix_now;
use crate::hook_run::transcript;
use llmenv_mcp::mcp_client::McpHttpClient;

mod cut;
mod failure;
mod local;
mod outcome;
mod scope;
mod session_reset;
mod subagent;
mod waves;

#[cfg(test)]
use cut::cut_items_notice;
use cut::{drop_cut_items, join_sections, note_cut_items, pack_bytes};
pub(super) use failure::tool_failure;
pub(super) use local::record_local;
use outcome::Outcome;
use scope::run_scope;
#[cfg(test)]
use scope::{scope_cut_notice, unrun_notice};
use session_reset::{FirstUpdate, first_update_notice, resets_ledger};
use subagent::agent_key;
pub(super) use subagent::subagent_start;
use waves::waves;

const FAILURE_BUDGET_BYTES: usize = 2_000;
const SUBAGENT_BUDGET_BYTES: usize = 4_000;
const MAIN_LIMIT: u8 = 10;
const FANOUT_LIMIT: u8 = 3;
const FAILURE_LIMIT: u8 = 5;
const TAIL_CHARS: usize = 300;
/// Wave 2 would push a slow backend past the latency that a prompt tolerates.
const WAVE2_CUTOFF: Duration = Duration::from_millis(1_500);
/// The mark icm puts at the end of a record it cut short.
const CUT_MARKER: &str = "[…]";
/// The topic of the fixes that work in every project.
const RESOLVED_TOPIC: &str = "errors-resolved";
/// The topic prefix of a scope-context record. The scope set already sends these.
const SCOPE_RECORD: &str = "[llmenv-scope-context]";

/// What every adaptive flow needs.
pub(super) struct AdaptiveCtx<'a> {
    pub(super) client: &'a McpHttpClient,
    pub(super) store: &'a LedgerStore,
    pub(super) session_id: &'a str,
    pub(super) payload: &'a Value,
    /// The topics that the main recall reads across every project (#2950).
    pub(super) global_topics: &'a [String],
    /// Where the recall health ring lives (#2919).
    pub(super) state_dir: &'a Path,
}

/// A recall across all projects. ICM's default project filter is the ICM
/// server's own cwd, which says nothing about this session when ICM runs remotely.
fn query(text: &str, limit: u8) -> RecallQuery {
    RecallQuery {
        query: text.to_string(),
        topic: None,
        keyword: None,
        project: Some(String::new()),
        limit,
    }
}

/// The notice for a flow whose every ICM call failed. A single failed call
/// only costs its records, but a dead backend must be visible, as it is on the
/// stateless path.
fn outage_notice(flow: &str, outcome: &Outcome) -> Option<String> {
    outcome.all_failed().then(|| {
        format!(
            "llmenv: memory {flow} recall skipped: all {} ICM calls failed. Fix: run `llmenv \
             doctor` to check the memory server.",
            outcome.calls
        )
    })
}

/// Print the outage line and the `[LLMENV_CONTEXT]` trace line for one flow.
fn report(ctx: &AdaptiveCtx<'_>, flow: &str, outcome: &Outcome, budget: &RecallBudget) {
    let ok = outcome.calls.saturating_sub(outcome.failed);
    crate::hook_run::recall_health::record(ctx.state_dir, ok, outcome.failed, unix_now());
    // The engine drops a hook's stderr, so the outage goes to the agent as a notice.
    if let Some(line) = outage_notice(flow, outcome) {
        agent_notice::record_logged(line);
    }
    if let Some(line) = budget.trace_line(crate::trace::enabled()) {
        crate::trace::emit_trace_line(&line);
    }
}

/// Print the `[LLMENV_LEDGER]` trace line for the ledger update of one flow (#3012).
fn report_ledger(flow: &str, timing: &LedgerTiming) {
    if crate::trace::enabled() {
        crate::trace::emit_trace_line(&crate::trace::lines::ledger_flow_line(
            flow,
            crate::trace::lines::LedgerFields {
                lock_attempts: timing.lock_attempts,
                lock_wait_us: timing.lock_wait_us,
                read_us: timing.read_us,
                write_us: timing.write_us,
                bytes: timing.ledger_bytes,
                state_unsaved: timing.state_unsaved,
            },
        ));
    }
}

async fn recall(client: &McpHttpClient, q: Option<RecallQuery>) -> Option<anyhow::Result<Reply>> {
    Some(Action::RecallQuery(q?).run(client, "", "").await)
}

/// The session project's own memories for the wake-up pack, and whether the call answered. The
/// icm wake-up fills its budget with global records first, so this project-scoped recall runs
/// beside it (#2756).
async fn project_memories(client: &McpHttpClient, project: &str) -> (String, bool) {
    let q = RecallQuery {
        project: Some(project.to_string()),
        ..query(project, MAIN_LIMIT)
    };
    let answered = Action::RecallQuery(q).run(client, "", "").await;
    match answered.and_then(Reply::into_records) {
        Ok(records) => {
            let records: Vec<String> = records
                .into_iter()
                .map(|r| r.text)
                .filter(|r| !r.starts_with(SCOPE_RECORD))
                .collect();
            let (kept, cut) = drop_cut_items(&records.join("\n"));
            note_cut_items(cut);
            (kept, true)
        }
        Err(e) => {
            tracing::error!("session start: project memories not recalled for {project}: {e:#}");
            agent_notice::record(format!(
                "llmenv: project memories were skipped at session start: icm recall failed ({}). \
                 Fix: run `llmenv doctor` to check the memory server.",
                llmenv_mcp::stdio_rpc::tidy_reason(&format!("{e:#}"))
            ));
            (String::new(), false)
        }
    }
}

/// Notice for a context reset that the ledger did not apply. Records seen before the
/// reset stay filtered from recall, so the agent must hear about it.
const RESET_UNDONE: &str = "llmenv: the recall ledger was not reset after a context reset (the \
     state file was locked or unreadable). Records seen before the reset may stay filtered from \
     recall. Fix: run `llmenv doctor` to check the state directory.";

/// `SessionStart`: reset per `source`, then send the wake-up pack and the scope set.
/// A continued session (resume, fork) gets no wake-up pack (#2142).
pub(super) async fn session_start(
    ctx: &AdaptiveCtx<'_>,
    wake: Action,
    scope: Vec<Action>,
) -> anyhow::Result<super::HookText> {
    let reset = resets_ledger(ctx.payload["source"].as_str());
    let project = match &wake {
        Action::WakeUp(args) => args.project.clone(),
        _ => None,
    };
    let recall_project = project.clone();
    // One locked update, so a busy lock can never leave a pre-reset sent set in use.
    let resume = super::resume_cache::ResumeCache::from_payload(ctx.payload);
    let (updated, ledger_timing) = ctx.store.update_reporting_save(ctx.session_id, |l| {
        l.resume_cache = Some(resume);
        if reset {
            l.reset();
        }
        if project.is_some() {
            l.project = project;
        }
        l.sent_for(MAIN_AGENT)
    });
    if let Some(notice) = first_update_notice(FirstUpdate::of(updated.as_ref()), reset) {
        tracing::error!("session start ledger update failed (reset requested: {reset}): {notice}");
        agent_notice::record(notice);
    }
    let sent = updated.map_or_else(BTreeSet::new, |(sent, _saved)| sent);
    // A resumed session already holds its pack, so it makes neither call (#2142). A failed
    // wake-up call must not discard the project memories that did load, so they are kept on the
    // error path below (#2756).
    let (wake, own) = if super::continues_session(ctx.payload) {
        (Ok(String::new()), String::new())
    } else {
        let (own, own_ok) = match &recall_project {
            Some(p) => {
                let (text, ok) = project_memories(ctx.client, p).await;
                (text, Some(ok))
            }
            None => (String::new(), None),
        };
        let wake = wake.run(ctx.client, "", "").await.map(|reply| {
            let (kept, cut) = drop_cut_items(&reply.into_text());
            note_cut_items(cut);
            join_sections(&kept, &own)
        });
        // These two calls are not in the scope outcome, so the health ring counts them here.
        let results = [Some(wake.is_ok()), own_ok];
        let ok = results.iter().filter(|r| **r == Some(true)).count();
        let failed = results.iter().filter(|r| **r == Some(false)).count();
        crate::hook_run::recall_health::record(ctx.state_dir, ok, failed, unix_now());
        (wake, own)
    };
    // Claude Code spills output over about 10 KB to a file, so the scope set gets
    // only the room that the wake-up pack leaves.
    let wake_bytes = pack_bytes(&wake, &own);
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES.saturating_sub(wake_bytes), sent);
    let scope_len = scope.len();
    let (outcome, unrun) = run_scope(ctx, scope, &mut budget).await;
    if let Some(notice) = unrun {
        agent_notice::record(notice);
    }
    report(ctx, "session_start", &outcome, &budget);
    report_ledger("session_start", &ledger_timing);
    let wake = match wake {
        Ok(text) => text,
        Err(e) if (outcome.all_failed() || scope_len == 0) && own.is_empty() => return Err(e),
        Err(e) => {
            // The agent is told: stderr from a hook never reaches it.
            tracing::error!(
                "session start: icm_wake_up failed, the wake-up pack is skipped: {e:#}"
            );
            agent_notice::record(format!(
                "llmenv: memory wake-up skipped: icm_wake_up failed ({}); the scope set still \
                 goes out. Fix: run `llmenv doctor` to check the memory server.",
                llmenv_mcp::stdio_rpc::tidy_reason(&format!("{e:#}"))
            ));
            own
        }
    };
    let complete = outcome.failed == 0;
    let kept = budget.kept_keys();
    note_unless_saved(
        ctx.store.update(ctx.session_id, |l| {
            l.mark_sent(MAIN_AGENT, kept);
            if complete {
                l.set_scope_sent(MAIN_AGENT);
            }
        }),
        RECALL_STATE_UNSAVED,
    );
    let passthrough = if wake.is_empty() {
        Vec::new()
    } else {
        vec![wake]
    };
    Ok(budget.hook_text(passthrough))
}

/// Notice for a ledger update that did not run: the next recall may repeat records (#2729).
const RECALL_STATE_UNSAVED: &str = "llmenv: recall state was not saved (the state file was locked \
     or unreadable), so some records may repeat. Fix: run `llmenv doctor` to check the state \
     directory.";

/// Notice for tool activity that was not recorded: a later recall runs without it (#2729).
const ACTIVITY_UNSAVED: &str = "llmenv: recent tool activity was not saved (the state file was \
     locked or unreadable), so later recall may miss it. Fix: run `llmenv doctor` to check the \
     state directory.";

/// Tells the agent when a ledger update did not run (unsafe id, busy lock, or unreadable file).
fn note_unless_saved<T>(saved: Option<T>, notice: &str) {
    if saved.is_none() {
        agent_notice::record(notice);
    }
}

/// Tells the agent that recall was skipped, so it is not read as "nothing relevant" (#2664).
fn note_skipped() {
    agent_notice::record(
        "llmenv: recall was skipped: the recall ledger cannot be read or updated. Fix: run \
         `llmenv doctor` to check the state directory.",
    );
}

/// The session's recall ledger, or a logged reason to skip recall. An empty default would forget
/// what this session already received, so the hook injects nothing rather than repeat it (#2711).
fn ledger_or_skip(store: &LedgerStore, session_id: &str, hook: &str) -> Option<Ledger> {
    let ledger = store.load(session_id);
    if ledger.is_none() {
        tracing::error!(
            "{hook}: recall skipped, the recall ledger cannot be read \
             (unsafe session id, busy lock, or unreadable file)"
        );
        note_skipped();
    }
    ledger
}

/// `TurnStart`: relevance recall, with the scope set as a fallback when no
/// `SessionStart` ran in this epoch.
pub(super) async fn turn_start(
    ctx: &AdaptiveCtx<'_>,
    scope: Vec<Action>,
) -> anyhow::Result<super::HookText> {
    let Some(ledger) = ledger_or_skip(ctx.store, ctx.session_id, "turn_start") else {
        return Ok(super::HookText::default());
    };
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, ledger.sent_for(MAIN_AGENT));
    let fallback = !ledger.scope_sent(MAIN_AGENT);
    let mut outcome = Outcome::default();
    if fallback {
        let (scope_outcome, unrun) = run_scope(ctx, scope, &mut budget).await;
        if let Some(notice) = unrun {
            agent_notice::record(notice);
        }
        outcome = scope_outcome;
    }
    let tail = ctx.payload["transcript_path"]
        .as_str()
        .and_then(|p| transcript::last_assistant_text(Path::new(p), TAIL_CHARS));
    let text = relevance::turn_query(&TurnSignals {
        prompt: ctx.payload["prompt"].as_str().unwrap_or_default(),
        activity: ledger.activity(),
        errors: ledger.errors(),
        last_turn_at: ledger.last_turn_at,
        assistant_tail: tail.as_deref(),
    });
    let query_hash = record_hash(&text);
    let repeated = ledger.last_query_hash.as_deref() == Some(query_hash.as_str())
        && !ledger.activity_since(ledger.last_turn_at);
    let mut main_ok = false;
    if !repeated && !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        let (wave, ok) = waves(
            ctx,
            &text,
            &keywords,
            ledger.project.as_deref(),
            &mut budget,
        )
        .await;
        main_ok = ok;
        outcome.merge(wave);
    }
    report(ctx, "turn_start", &outcome, &budget);
    let kept = budget.kept_keys();
    let now = unix_now();
    let (updated, timing) = ctx.store.update_timed(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        l.last_turn_at = now;
        // A failed recall is not a finished query; the skip rule must not reuse it.
        if main_ok {
            l.last_query_hash = Some(query_hash);
        }
        // The fallback is tried once per session epoch. A failed try is not repeated on
        // every turn: the outage would repeat it, and the SessionStart pack is the main path.
        if fallback {
            l.set_scope_sent(MAIN_AGENT);
        }
    });
    note_unless_saved(updated, RECALL_STATE_UNSAVED);
    report_ledger("turn_start", &timing);
    Ok(budget.hook_text(Vec::new()))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::expect_fun_call)]
mod skip_notice_tests {
    use super::{LedgerStore, ledger_or_skip, note_unless_saved};

    #[test]
    fn a_skipped_recall_tells_the_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LedgerStore::new(dir.path());
        assert!(ledger_or_skip(&store, "../unsafe", "turn_start").is_none());
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(
            text.contains("recall was skipped: the recall ledger"),
            "got: {text}"
        );
    }

    #[test]
    fn an_update_that_did_not_run_tells_the_agent_and_a_saved_one_does_not() {
        note_unless_saved(None::<()>, "adaptive test: update did not run");
        note_unless_saved(Some(()), "adaptive test: update was saved");
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(
            text.contains("adaptive test: update did not run"),
            "got: {text}"
        );
        assert!(
            !text.contains("adaptive test: update was saved"),
            "got: {text}"
        );
    }
}
