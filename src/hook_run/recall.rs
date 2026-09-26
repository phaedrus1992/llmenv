//! Recall ordering and the byte budget for the `TurnStart` hook (#2159).
//!
//! Claude Code saves hook output above about 10 KB to a file and shows the model a 2 KB
//! preview, so the most specific recalls must come first and the total must stay small.
//! Design: docs/design/issue-2159-2141-icm-recall-prioritization.md

use std::collections::{BTreeMap, HashSet};
use std::future::Future;

use crate::hook_run::action::{Action, split_recall_records};

/// Claude Code saves hook output over about 10 KB to a file and shows the model a 2 KB
/// preview. Stay well under that limit.
const RECALL_BUDGET_BYTES: usize = 8_000;

/// Below this many free bytes the remaining recall queries are skipped, because their
/// records would rarely fit.
const MIN_FREE_BYTES: usize = 200;

/// Bytes the `"\n\n"` join adds after each kept record.
const SEPARATOR_BYTES: usize = 2;

/// The rank of a tag no scope contributed, such as the OS tag.
pub(super) const UNSCOPED_RANK: u8 = 6;

/// The rank of every tag from a project scope or `$LLMENV_EXTRA_TAGS`, and of every bundle.
pub(super) const MOST_SPECIFIC_RANK: u8 = 1;

/// Rank each active tag by how specific its source scope is. A lower rank is more specific
/// and is recalled first. A tag from several scopes takes the lowest rank among them.
pub(super) fn tag_specificity(active: &crate::scope::ActiveScopes) -> BTreeMap<String, u8> {
    let mut ranks: BTreeMap<String, u8> = active
        .tags
        .iter()
        .map(|tag| (tag.clone(), UNSCOPED_RANK))
        .collect();
    let mut lower = |tag: &String, rank: u8| {
        ranks
            .entry(tag.clone())
            .and_modify(|current| *current = (*current).min(rank))
            .or_insert(rank);
    };
    for scope in &active.scopes {
        let rank = match scope.kind {
            "project" => 1,
            "content" => 2,
            "network" => 3,
            "user" => 4,
            "host" => 5,
            _ => UNSCOPED_RANK,
        };
        scope.tags.iter().for_each(|tag| lower(tag, rank));
    }
    // `extra_tags` stand in for a project's `.llmenv.yaml`, so they rank as project tags.
    active
        .extra_tags
        .iter()
        .for_each(|tag| lower(tag, MOST_SPECIFIC_RANK));
    ranks
}

/// The recall records kept so far, and what did not fit.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RecallBudget {
    kept: Vec<String>,
    seen: HashSet<String>,
    kept_bytes: usize,
    /// Records seen, including duplicates and omitted ones.
    pub(super) records: usize,
    /// Bytes of the records seen.
    pub(super) record_bytes: usize,
    /// Records identical to a record already kept.
    pub(super) duplicates: usize,
    /// Records dropped because they did not fit.
    pub(super) omitted: usize,
    /// Recall actions not run because the budget was full.
    pub(super) skipped_actions: usize,
}

impl RecallBudget {
    /// Whether too little room is left to run another recall.
    fn is_full(&self) -> bool {
        RECALL_BUDGET_BYTES.saturating_sub(self.kept_bytes) < MIN_FREE_BYTES
    }

    /// Keep each record that is new and fits. A record that does not fit is counted as
    /// omitted, and a later smaller record can still fit.
    fn add_records(&mut self, records: Vec<String>) {
        for record in records {
            self.records += 1;
            self.record_bytes += record.len();
            if self.seen.contains(&record) {
                self.duplicates += 1;
            } else if self.kept_bytes + record.len() + SEPARATOR_BYTES <= RECALL_BUDGET_BYTES {
                self.kept_bytes += record.len() + SEPARATOR_BYTES;
                self.seen.insert(record.clone());
                self.kept.push(record);
            } else {
                self.omitted += 1;
            }
        }
    }

    pub(super) fn kept(&self) -> &[String] {
        &self.kept
    }

    /// The `[LLMENV_CONTEXT]` trace line, or `None` when tracing is off or no recall ran or
    /// was skipped.
    ///
    /// `recall_*` counts every record ICM returned and `injected_*` counts the records kept.
    /// `advisory_stripped` counts duplicate records, `omitted` counts records dropped for the
    /// byte budget, and `skipped_actions` counts recall queries not run because the budget was
    /// full.
    pub(super) fn trace_line(&self, enabled: bool) -> Option<String> {
        if !enabled || (self.records == 0 && self.skipped_actions == 0) {
            return None;
        }
        let injected_bytes: usize = self.kept.iter().map(String::len).sum();
        Some(format!(
            "[LLMENV_CONTEXT] recall_entries={} recall_bytes={} injected_entries={} \
             injected_bytes={injected_bytes} advisory_stripped={} omitted={} skipped_actions={}",
            self.records,
            self.record_bytes,
            self.kept.len(),
            self.duplicates,
            self.omitted,
            self.skipped_actions
        ))
    }

    /// One line that says what was left out, or `None` when nothing was.
    fn notice(&self) -> Option<String> {
        let (omitted, skipped) = (self.omitted, self.skipped_actions);
        if skipped > 0 {
            Some(format!(
                "[llmenv] {omitted} lower-priority memories omitted; {skipped} recall queries \
                 skipped to stay under the context limit."
            ))
        } else if omitted > 0 {
            Some(format!(
                "[llmenv] {omitted} lower-priority memories omitted to stay under the context \
                 limit."
            ))
        } else {
            None
        }
    }
}

fn is_recall(action: &Action) -> bool {
    matches!(
        action,
        Action::Recall | Action::RecallTag(_) | Action::RecallBundle(_)
    )
}

/// Run `actions` in order through `run`, and join their text.
///
/// Recall text is split into records, deduplicated by record, and kept in order until
/// [`RECALL_BUDGET_BYTES`] is used. Other actions pass through unchanged. Returns the text and
/// the budget, for the trace line.
///
/// # Errors
/// Returns the first error from `run`.
pub(super) async fn run_with_budget<F, Fut>(
    actions: Vec<Action>,
    mut run: F,
) -> anyhow::Result<(String, RecallBudget)>
where
    F: FnMut(Action) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let mut budget = RecallBudget::default();
    let mut passthrough: Vec<String> = Vec::new();
    for action in actions {
        if !is_recall(&action) {
            let text = run(action).await?;
            if !text.is_empty() && !passthrough.contains(&text) {
                passthrough.push(text);
            }
        } else if budget.is_full() {
            budget.skipped_actions += 1;
        } else {
            budget.add_records(split_recall_records(&run(action).await?));
        }
    }
    let mut parts = passthrough;
    parts.extend(budget.kept().iter().cloned());
    parts.extend(budget.notice());
    Ok((parts.join("\n\n"), budget))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use std::cell::Cell;

    use proptest::prelude::*;

    use super::*;
    use crate::scope::{ActiveScope, ActiveScopes};

    fn scope(kind: &'static str, tags: &[&str]) -> ActiveScope {
        ActiveScope {
            kind,
            tags: tags.iter().map(ToString::to_string).collect(),
            ..ActiveScope::default()
        }
    }

    fn active(scopes: Vec<ActiveScope>, extra: &[&str], loose: &[&str]) -> ActiveScopes {
        let mut tags: std::collections::BTreeSet<String> =
            scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
        tags.extend(extra.iter().map(ToString::to_string));
        tags.extend(loose.iter().map(ToString::to_string));
        ActiveScopes {
            scopes,
            tags,
            extra_tags: extra.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn tag_specificity_ranks_by_scope_kind() {
        let scopes = vec![
            scope("project", &["proj", "shared"]),
            scope("content", &["cont"]),
            scope("network", &["net"]),
            scope("user", &["usr"]),
            scope("host", &["hst", "shared"]),
        ];
        let ranks = tag_specificity(&active(scopes, &["extra"], &["macos"]));
        let expected: BTreeMap<String, u8> = [
            ("proj", 1),
            ("shared", 1),
            ("cont", 2),
            ("net", 3),
            ("usr", 4),
            ("hst", 5),
            ("extra", 1),
            ("macos", 6),
        ]
        .into_iter()
        .map(|(tag, rank)| (tag.to_string(), rank))
        .collect();
        assert_eq!(ranks, expected);
    }

    fn records(count: usize, size: usize, tag: &str) -> String {
        (0..count)
            .map(|i| format!("[t] {tag}{i:04}{}", "x".repeat(size.saturating_sub(9))))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn run_texts(texts: Vec<String>) -> (String, RecallBudget) {
        let actions = texts.iter().map(|_| Action::Recall).collect();
        let index = Cell::new(0);
        run_with_budget(actions, |_| {
            let text = texts[index.get()].clone();
            index.set(index.get() + 1);
            async move { Ok(text) }
        })
        .await
        .expect("fake runner does not fail")
    }

    #[tokio::test]
    async fn budget_keeps_whole_records_and_reports_the_omitted_count() {
        let (text, budget) = run_texts(vec![records(30, 500, "a")]).await;
        let notice = text.lines().last().expect("has a notice");
        assert!(
            text.len() <= RECALL_BUDGET_BYTES + notice.len() + SEPARATOR_BYTES,
            "{}",
            text.len()
        );
        assert_eq!(budget.kept().len() + budget.omitted, 30);
        assert!(budget.omitted > 0);
        assert_eq!(
            notice,
            format!(
                "[llmenv] {} lower-priority memories omitted to stay under the context limit.",
                budget.omitted
            )
        );
        assert!(
            budget.kept().iter().all(|r| r.len() == 500),
            "whole records only"
        );
    }

    #[tokio::test]
    async fn a_duplicate_record_is_kept_once_at_its_first_position() {
        let (text, budget) = run_texts(vec![
            "[t] shared\n[t] one".to_string(),
            "[t] two\n[t] shared".to_string(),
        ])
        .await;
        assert_eq!(text, "[t] shared\n\n[t] one\n\n[t] two");
        assert_eq!(budget.duplicates, 1);
        assert_eq!(budget.omitted, 0);
    }

    #[tokio::test]
    async fn a_small_record_still_fits_after_a_large_one_is_omitted() {
        let big = format!("[t] {}", "b".repeat(RECALL_BUDGET_BYTES));
        let (text, budget) = run_texts(vec![format!("[t] first\n{big}\n[t] small")]).await;
        assert!(text.starts_with("[t] first\n\n[t] small"), "{text}");
        assert_eq!(budget.omitted, 1);
    }

    #[tokio::test]
    async fn later_recalls_are_skipped_once_the_budget_is_full() {
        let calls = Cell::new(0);
        let actions = vec![Action::Recall, Action::Recall, Action::Recall];
        let (text, budget) = run_with_budget(actions, |_| {
            calls.set(calls.get() + 1);
            async {
                Ok(format!(
                    "[t] {}\n[t] {}",
                    "a".repeat(7_900),
                    "b".repeat(500)
                ))
            }
        })
        .await
        .expect("fake runner does not fail");
        assert_eq!(calls.get(), 1, "budget was full after the first recall");
        assert_eq!(budget.skipped_actions, 2);
        assert_eq!(budget.omitted, 1);
        assert!(
            text.ends_with(&format!(
                "[llmenv] {} lower-priority memories omitted; 2 recall queries skipped to stay \
                 under the context limit.",
                budget.omitted
            )),
            "{text}"
        );
    }

    #[tokio::test]
    async fn non_recall_actions_pass_through_outside_the_budget() {
        let big = "w".repeat(RECALL_BUDGET_BYTES + 1000);
        let (text, budget) = run_with_budget(vec![Action::WakeUp(None)], |_| {
            let big = big.clone();
            async move { Ok(big) }
        })
        .await
        .expect("fake runner does not fail");
        assert_eq!(text, big);
        assert_eq!(budget, RecallBudget::default());
    }

    fn record_of(len: usize) -> String {
        format!("[t] {}", "x".repeat(len - 4))
    }

    #[test]
    fn the_budget_is_used_exactly_and_never_exceeded() {
        let mut budget = RecallBudget::default();
        // Two records use 3,002 bytes each with the separator, so 1,994 bytes of room remain.
        budget.add_records(vec![record_of(3_000), format!("{}y", record_of(2_999))]);
        budget.add_records(vec![record_of(1_995), record_of(1_994)]);
        let lengths: Vec<usize> = budget.kept().iter().map(String::len).collect();
        assert_eq!(lengths, [3_000, 3_000, 1_994]);
        assert_eq!(budget.omitted, 1);
    }

    #[test]
    fn a_full_budget_starts_at_exactly_the_minimum_free_bytes() {
        let mut budget = RecallBudget::default();
        // 7,798 + 2 separator bytes leaves exactly MIN_FREE_BYTES free: not full yet.
        budget.add_records(vec![record_of(
            RECALL_BUDGET_BYTES - MIN_FREE_BYTES - SEPARATOR_BYTES,
        )]);
        assert!(!budget.is_full());
        budget.add_records(vec![record_of(6)]);
        assert!(budget.is_full());
    }

    #[test]
    fn trace_line_is_absent_until_a_recall_ran() {
        assert_eq!(RecallBudget::default().trace_line(true), None);
        let skipped_only = RecallBudget {
            skipped_actions: 2,
            ..RecallBudget::default()
        };
        assert!(skipped_only.trace_line(true).is_some());
        assert_eq!(skipped_only.trace_line(false), None);
    }

    #[test]
    fn trace_line_reports_every_counter() {
        let mut budget = RecallBudget::default();
        budget.add_records(vec![
            "[t] a".to_string(),
            "[t] a".to_string(),
            "[t] bb".to_string(),
        ]);
        assert_eq!(
            budget.trace_line(true).as_deref(),
            Some(
                "[LLMENV_CONTEXT] recall_entries=3 recall_bytes=16 injected_entries=2 \
                 injected_bytes=11 advisory_stripped=1 omitted=0 skipped_actions=0"
            )
        );
    }

    #[tokio::test]
    async fn passthrough_text_skips_empty_results_and_exact_duplicates() {
        let texts = ["", "pack", "pack", "other"];
        let index = Cell::new(0);
        let (text, _) = run_with_budget(vec![Action::WakeUp(None); 4], |_| {
            let text = texts[index.get()].to_string();
            index.set(index.get() + 1);
            async move { Ok(text) }
        })
        .await
        .expect("fake runner does not fail");
        assert_eq!(text, "pack\n\nother");
    }

    #[tokio::test]
    async fn a_failing_action_propagates_its_error() {
        let result = run_with_budget(vec![Action::Recall], |_| async {
            Err::<String, _>(anyhow::anyhow!("boom"))
        })
        .await;
        assert!(result.is_err());
    }

    proptest! {
        /// Whatever the records, the kept text stays within budget and every kept record is
        /// byte-identical to an input record.
        #[test]
        fn kept_records_fit_and_come_from_the_input(
            input in prop::collection::vec("[a-z ]{1,900}", 0..40)
        ) {
            let input: Vec<String> = input
                .into_iter()
                .map(|body| format!("[t] {}", body.trim_end()))
                .collect();
            let mut budget = RecallBudget::default();
            budget.add_records(input.clone());
            let kept_bytes: usize = budget.kept().iter().map(|r| r.len() + SEPARATOR_BYTES).sum();
            prop_assert!(kept_bytes <= RECALL_BUDGET_BYTES);
            for record in budget.kept() {
                prop_assert!(input.contains(record));
            }
            prop_assert_eq!(
                budget.kept().len() + budget.omitted + budget.duplicates,
                input.len()
            );
        }
    }
}
