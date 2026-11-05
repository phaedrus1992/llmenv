//! Adaptive recall for `SubagentStart` (#2249).
//!
//! The subagent flow lives here so `adaptive.rs` keeps the shared recall helpers and the
//! main-agent flows. Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use serde_json::Value;

use super::{
    AdaptiveCtx, Outcome, RECALL_STATE_UNSAVED, SUBAGENT_BUDGET_BYTES, ledger_or_skip,
    note_skipped, note_unless_saved, report, report_ledger, waves,
};
use crate::agent_notice;
use crate::hook_run::HookText;
use crate::hook_run::recall::RecallBudget;
use crate::hook_run::relevance;
use crate::hook_run::session_ledger::MAIN_AGENT;
use crate::hook_run::session_state::unix_now;

/// The ledger key for the context that fired the hook, and whether hashes may be
/// recorded under it. An unsafe `agent_id` filters as `main` and records nothing.
pub(super) fn agent_key(payload: &Value) -> (String, bool) {
    match payload["agent_id"].as_str() {
        None => (MAIN_AGENT.to_string(), true),
        Some(id) if crate::paths::is_valid_short_name(id) => (id.to_string(), true),
        Some(_) => (MAIN_AGENT.to_string(), false),
    }
}

/// `SubagentStart`: task-relevant memories for a fresh subagent context.
pub(in crate::hook_run) async fn subagent_start(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<HookText> {
    let (key, record) = agent_key(ctx.payload);
    if !record || key == MAIN_AGENT {
        agent_notice::record(
            "llmenv: subagent memories were not injected: the SubagentStart payload has no \
             usable agent_id. Fix: run `llmenv doctor`; if this repeats, report the hook log.",
        );
        return Ok(HookText::default());
    }
    let agent_type = ctx.payload["agent_type"].as_str().unwrap_or_default();
    let now = unix_now();
    // Checked before the take below, which consumes the pending task for good. A failed read
    // after the take would lose that task, and SubagentStart does not fire again.
    if ledger_or_skip(ctx.store, ctx.session_id, "subagent_start").is_none() {
        return Ok(HookText::default());
    }
    // A resumed subagent already has an entry; its task was taken at launch, and
    // the queue now holds tasks for new siblings only.
    let taken = ctx.store.update(ctx.session_id, |l| {
        if l.knows_agent(&key) {
            None
        } else {
            l.take_subagent(agent_type, now)
        }
    });
    // A failed take is not "no queued task": the subagent query then runs without its task.
    if taken.is_none() {
        note_skipped();
    }
    let task = taken.flatten();
    let Some(ledger) = ledger_or_skip(ctx.store, ctx.session_id, "subagent_start") else {
        return Ok(HookText::default());
    };
    let text = relevance::subagent_query(
        task.as_ref().map(|t| t.task.as_str()),
        agent_type,
        ledger.activity(),
    );
    let mut budget = RecallBudget::new(SUBAGENT_BUDGET_BYTES, ledger.sent_for(&key));
    let mut outcome = Outcome::default();
    if !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        (outcome, _) = waves(
            ctx,
            &text,
            &keywords,
            ledger.project.as_deref(),
            &mut budget,
        )
        .await;
    }
    report(ctx, "subagent_start", &outcome, &budget);
    let kept = budget.kept_keys();
    let (updated, timing) = ctx
        .store
        .update_timed(ctx.session_id, |l| l.mark_sent(&key, kept));
    note_unless_saved(updated, RECALL_STATE_UNSAVED);
    report_ledger("subagent_start", &timing);
    Ok(budget.hook_text(Vec::new()))
}
