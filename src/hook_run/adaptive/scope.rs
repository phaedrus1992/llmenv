//! The scope set of the adaptive recall: the scope-tagged recalls that run until the output
//! budget is full.

use crate::hook_run::action::Action;
use crate::hook_run::adaptive::AdaptiveCtx;
use crate::hook_run::adaptive::outcome::Outcome;
use crate::hook_run::recall::RecallBudget;

/// The notice for scope recalls that the output budget cut off, or `None` when none were cut
/// (#2766). The recalls are unknown to the agent, not empty, so the notice says so.
pub(super) fn scope_cut_notice(skipped: usize) -> Option<String> {
    (skipped > 0).then(|| {
        format!(
            "llmenv: {skipped} recall action(s) were skipped because the output budget was full. \
             Those recalls are unknown, not empty. Fix: store shorter memories, so more recalls \
             fit the budget."
        )
    })
}

/// Run the scope recalls into `budget`, one after another, until it is full.
pub(super) async fn run_scope(
    ctx: &AdaptiveCtx<'_>,
    scope: Vec<Action>,
    budget: &mut RecallBudget,
) -> (Outcome, Option<String>) {
    let mut outcome = Outcome::default();
    let mut actions = scope.into_iter();
    // A refused or timed-out server would stall each remaining tag for one HOOK_TIMEOUT, so the
    // first failure ends the fallback for this turn (#2897).
    while !budget.is_full() && outcome.failed == 0 {
        let Some(action) = actions.next() else {
            break;
        };
        outcome.push(Some(action.run(ctx.client, "", "").await));
        budget.add_records(std::mem::take(&mut outcome.records));
    }
    let notice = unrun_notice(actions.len(), outcome.failed > 0, budget.is_full());
    (outcome, notice)
}

/// The notice for scope recalls that did not run, or `None` when none were skipped. A stop after
/// a failure is not retried this epoch, so the agent must hear about it (#2897). A cut by the
/// output budget has its own notice (#2766).
pub(super) fn unrun_notice(unrun: usize, failed: bool, budget_full: bool) -> Option<String> {
    if unrun == 0 {
        return None;
    }
    if budget_full {
        return scope_cut_notice(unrun);
    }
    failed.then(|| {
        format!(
            "llmenv: {unrun} scope recall action(s) did not run after a memory recall failed. \
             Fix: run `llmenv doctor` to check the memory server."
        )
    })
}
