//! The recall waves of one prompt: the project recall and the global topic recall, then the
//! keyword and topic fanouts.

use std::time::Instant;

use super::{AdaptiveCtx, FANOUT_LIMIT, MAIN_LIMIT, Outcome, WAVE2_CUTOFF, query, recall};
use crate::hook_run::action::{RecallQuery, Reply};
use crate::hook_run::recall::RecallBudget;
use crate::hook_run::relevance;

/// Recalls `text` into `budget`, in budget order: the session project's main recall, the
/// recall of the global topics, keyword fanout, topic fanout. The flag says whether the main
/// recalls ran and none of them failed. Only the global topics are read across projects, so
/// the records of another project stay out (#2950). With no session project, the fanouts do
/// not run.
///
/// The fanouts run only when the main recalls leave room in `budget`. Their records come
/// after the main records, so a full budget would drop them all (#2920). No call runs when
/// `budget` is full before the main recalls.
pub(super) async fn waves(
    ctx: &AdaptiveCtx<'_>,
    text: &str,
    keywords: &[String],
    project: Option<&str>,
    budget: &mut RecallBudget,
) -> (Outcome, bool) {
    let in_project = |q: RecallQuery| {
        project.map(|p| RecallQuery {
            project: Some(p.to_string()),
            ..q
        })
    };
    let client = ctx.client;
    // The scope fallback can fill the budget before this runs. Every record would then be
    // dropped, so no call runs. The query is not finished, so the next turn runs it again.
    if budget.is_full() {
        let has_global = ctx.global_topics.iter().any(|t| !t.trim().is_empty());
        let fanouts = project.map_or(0, |_| 1 + keywords.len().min(2));
        budget.skip_actions(usize::from(has_global) + fanouts);
        return (Outcome::default(), false);
    }
    let start = Instant::now();
    let (own, main) = tokio::join!(
        recall(client, in_project(query(text, MAIN_LIMIT))),
        crate::hook_run::global_topics::recall(client, text, ctx.global_topics, MAIN_LIMIT),
    );
    // A failed project recall is not a finished query, even when a global topic answered: the
    // skip rule would reuse it and the project's records would stay missing.
    let main_ok = (own.is_some() || main.is_some())
        && !matches!(own, Some(Err(_)))
        && !matches!(main, Some(Err(_)));
    let mut outcome = Outcome::default();
    push_into(&mut outcome, budget, [own, main]);
    let topics: Vec<String> = outcome
        .records
        .iter()
        .filter(|r| !r.topic.is_empty())
        .map(|r| r.topic.clone())
        .collect();
    let siblings = relevance::sibling_topics(&topics);
    let keyword = |i: usize| {
        keywords.get(i).and_then(|k| {
            in_project(RecallQuery {
                keyword: Some(k.clone()),
                ..query(text, FANOUT_LIMIT)
            })
        })
    };
    let topic = |i: usize| {
        siblings.get(i).and_then(|t| {
            in_project(RecallQuery {
                topic: Some(t.clone()),
                ..query(text, FANOUT_LIMIT)
            })
        })
    };
    let fanouts = [keyword(0), keyword(1), topic(0), topic(1)];
    if budget.is_full() {
        budget.skip_actions(fanouts.iter().flatten().count());
    } else if start.elapsed() < WAVE2_CUTOFF {
        let [k0, k1, t0, t1] = fanouts;
        let (k0, k1, t0, t1) = tokio::join!(
            recall(client, k0),
            recall(client, k1),
            recall(client, t0),
            recall(client, t1),
        );
        push_into(&mut outcome, budget, [k0, k1, t0, t1]);
    } else if fanouts.iter().any(Option::is_some) {
        // A slow server would push the prompt past the latency it tolerates. The skip is not a
        // budget skip, so it gets its own notice, not a line in the budget notice. With no
        // fanout to run, nothing is skipped, so there is no notice.
        crate::agent_notice::record_logged(format!(
            "llmenv: {} extra memory recall(s) were skipped, because the main recalls took \
             {:?} (limit {:?}). Fix: run `llmenv doctor` to check the memory server.",
            fanouts.iter().flatten().count(),
            start.elapsed(),
            WAVE2_CUTOFF
        ));
    }
    (outcome, main_ok)
}

/// Records each result in `outcome`, and adds each new record to `budget`.
fn push_into(
    outcome: &mut Outcome,
    budget: &mut RecallBudget,
    results: impl IntoIterator<Item = Option<anyhow::Result<Reply>>>,
) {
    let before = outcome.records.len();
    results.into_iter().for_each(|r| outcome.push(r));
    budget.add_records(outcome.records.iter().skip(before).cloned().collect());
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
