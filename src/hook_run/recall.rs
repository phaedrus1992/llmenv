//! Recall ordering and the byte budget for the `TurnStart` hook (#2159).
//!
//! Claude Code saves hook output above about 10 KB to a file and shows the model a 2 KB
//! preview, so the most specific recalls must come first and the total must stay small.
//! Design: docs/design/issue-2159-2141-icm-recall-prioritization.md

use std::collections::{BTreeSet, HashSet};
use std::future::Future;

use crate::hook_run::action::{Action, Recalled, Reply};
use crate::hook_run::session_ledger::record_hash;

/// Claude Code saves hook output over about 10 KB to a file and shows the model a 2 KB
/// preview. Stay well under that limit.
pub(super) const RECALL_BUDGET_BYTES: usize = 8_000;

/// Below this many free bytes the remaining recall queries are skipped, because their
/// records would rarely fit.
const MIN_FREE_BYTES: usize = 200;

/// Bytes the `"\n\n"` join adds after each kept record.
const SEPARATOR_BYTES: usize = 2;

mod rank;
pub(super) use rank::{MOST_SPECIFIC_RANK, UNSCOPED_RANK, tag_specificity};

/// The recall records kept so far, and what did not fit.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RecallBudget {
    kept: Vec<Recalled>,
    /// Keys of the records kept so far: the icm id, or the text hash of a record with no id.
    seen: HashSet<String>,
    kept_bytes: usize,
    /// Records seen, including duplicates and omitted ones.
    records: usize,
    /// Bytes of the records seen.
    record_bytes: usize,
    /// Records identical to a record already kept.
    duplicates: usize,
    /// Records dropped because they did not fit.
    omitted: usize,
    /// Recall actions not run because the budget was full.
    skipped_actions: usize,
    /// Byte limit of the kept records.
    limit: usize,
    /// Keys of records the context already holds (#2249): icm ids, and the text hashes that a
    /// ledger from before #3369 holds.
    sent: BTreeSet<String>,
    /// Records skipped because their key or text hash is in `sent`.
    already_sent: usize,
    /// Ids of records that matched `sent` by text hash only, so the ledger learns their ids.
    migrated: Vec<String>,
}

impl Default for RecallBudget {
    fn default() -> Self {
        Self::new(RECALL_BUDGET_BYTES, BTreeSet::new())
    }
}

impl RecallBudget {
    /// A budget of `limit` bytes that skips every record whose hash is in `sent`.
    pub(super) fn new(limit: usize, sent: BTreeSet<String>) -> Self {
        Self {
            kept: Vec::new(),
            seen: HashSet::new(),
            kept_bytes: 0,
            records: 0,
            record_bytes: 0,
            duplicates: 0,
            omitted: 0,
            skipped_actions: 0,
            limit,
            sent,
            already_sent: 0,
            migrated: Vec::new(),
        }
    }

    /// Whether too little room is left to run another recall.
    pub(super) fn is_full(&self) -> bool {
        self.limit.saturating_sub(self.kept_bytes) < MIN_FREE_BYTES
    }

    /// Counts `count` recall queries that did not run because the budget was full.
    pub(super) fn skip_actions(&mut self, count: usize) {
        self.skipped_actions += count;
    }

    /// Adds text that did not come from a JSON recall, split into records with no id.
    #[cfg(test)]
    pub(super) fn add_text(&mut self, text: &str) {
        self.add_records(Reply::Text(text.to_string()).into_records());
    }

    /// The keys to add to the ledger's sent set: each kept record, and each record that matched
    /// the ledger by its old text hash only. A ledger from before #3369 then moves to ids.
    pub(super) fn kept_keys(&self) -> Vec<String> {
        self.kept
            .iter()
            .map(Recalled::key)
            .chain(self.migrated.iter().cloned())
            .collect()
    }

    /// Whether the context already holds `record`. A record with an id also matches the text
    /// hash that a ledger from before #3369 holds; such a match is noted, so the ledger learns
    /// the id.
    fn already_sent(&mut self, record: &Recalled) -> bool {
        let key = record.key();
        if self.sent.contains(&key) {
            return true;
        }
        if record.id.is_some() && self.sent.contains(&record_hash(&record.text)) {
            self.migrated.push(key);
            return true;
        }
        false
    }

    /// The passthrough text, then the kept records. The omission notice is not here: it is llmenv
    /// text, so [`Self::hook_text`] sends it apart from the ICM memory (#2764).
    fn render(&self, passthrough: Vec<String>) -> String {
        let mut parts = passthrough;
        parts.extend(self.kept.iter().map(|r| r.text.clone()));
        parts.join("\n\n")
    }

    /// The memory text with the omission notice in `notes`, so the notice never gets the ICM
    /// header (#2764).
    pub(super) fn hook_text(&self, passthrough: Vec<String>) -> super::HookText {
        super::HookText {
            memory: self.render(passthrough),
            notes: self.notice().unwrap_or_default(),
        }
    }

    /// Keep each record that is new and fits. A record is new when its key is not kept yet:
    /// two icm records with the same text and different ids are both kept (#3369). A record
    /// that does not fit is counted as omitted, and a later smaller record can still fit.
    pub(super) fn add_records(&mut self, records: Vec<Recalled>) {
        for record in records {
            let bytes = record.text.len();
            self.records += 1;
            self.record_bytes += bytes;
            if self.already_sent(&record) {
                self.already_sent += 1;
            } else if self.seen.contains(&record.key()) {
                self.duplicates += 1;
            } else if self.kept_bytes + bytes + SEPARATOR_BYTES <= self.limit {
                self.kept_bytes += bytes + SEPARATOR_BYTES;
                self.seen.insert(record.key());
                self.kept.push(record);
            } else {
                self.omitted += 1;
            }
        }
    }

    /// Adds each string as one record with no id.
    #[cfg(test)]
    fn add_strings(&mut self, records: Vec<String>) {
        self.add_records(
            records
                .into_iter()
                .map(|text| Recalled { id: None, text })
                .collect(),
        );
    }

    #[cfg(test)]
    fn kept(&self) -> Vec<&str> {
        self.kept.iter().map(|r| r.text.as_str()).collect()
    }

    /// The `[LLMENV_CONTEXT]` trace line, or `None` when tracing is off or no recall ran or
    /// was skipped.
    ///
    /// `recall_*` counts every record ICM returned and `injected_*` counts the records kept.
    /// `duplicate_records` counts records identical to one kept earlier in this call,
    /// `already_sent` counts records skipped because the context already holds them,
    /// `omitted` counts records dropped for the byte budget, and `skipped_actions` counts recall queries not run because the budget was
    /// full.
    pub(super) fn trace_line(&self, enabled: bool) -> Option<String> {
        if !enabled || (self.records == 0 && self.skipped_actions == 0) {
            return None;
        }
        let injected_bytes: usize = self.kept.iter().map(|r| r.text.len()).sum();
        Some(format!(
            "[LLMENV_CONTEXT] recall_entries={} recall_bytes={} injected_entries={} \
             injected_bytes={injected_bytes} duplicate_records={} already_sent={} omitted={} \
             skipped_actions={}",
            self.records,
            self.record_bytes,
            self.kept.len(),
            self.duplicates,
            self.already_sent,
            self.omitted,
            self.skipped_actions
        ))
    }

    /// One line that says what was left out, or `None` when nothing was.
    pub(super) fn notice(&self) -> Option<String> {
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

/// Run `actions` in order through `run`, and join their text.
///
/// Recall text is split into records, deduplicated by record, and kept in order until
/// [`RECALL_BUDGET_BYTES`] is used. Other actions pass through unchanged. Returns the text and
/// the budget, for the trace line.
///
/// A failed action records an agent notice with its cause, and the other actions still run, so
/// the text they return is kept (#2736).
pub(super) async fn run_with_budget<F, Fut>(actions: Vec<Action>, run: F) -> (String, RecallBudget)
where
    F: FnMut(Action) -> Fut,
    Fut: Future<Output = anyhow::Result<Reply>>,
{
    run_with_budget_filtered(actions, RecallBudget::default(), run).await
}

/// [`run_with_budget`] with a caller-supplied budget, so that a limit and a sent
/// set apply (#2249).
///
/// # Errors
/// Returns the first error from `run`.
async fn run_with_budget_filtered<F, Fut>(
    actions: Vec<Action>,
    mut budget: RecallBudget,
    mut run: F,
) -> (String, RecallBudget)
where
    F: FnMut(Action) -> Fut,
    Fut: Future<Output = anyhow::Result<Reply>>,
{
    let mut passthrough: Vec<String> = Vec::new();
    for action in actions {
        let recall = action.is_recall();
        if recall && budget.is_full() {
            budget.skipped_actions += 1;
            continue;
        }
        let Ok(reply) = run(action).await.inspect_err(|e| {
            tracing::error!("memory action failed, its text is missing: {e:#}");
            crate::agent_notice::record(format!(
                "llmenv could not run a memory action, so its text is missing: {e:#}. \
                 Fix: run `llmenv doctor` to check the memory server."
            ));
        }) else {
            continue;
        };
        if recall {
            budget.add_records(reply.into_records());
        } else {
            let text = reply.into_text();
            if !text.is_empty() && !passthrough.contains(&text) {
                passthrough.push(text);
            }
        }
    }
    let text = budget.render(passthrough);
    (text, budget)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;

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
        let actions = texts
            .iter()
            .map(|_| Action::Recall(String::new()))
            .collect();
        let index = Cell::new(0);
        run_with_budget(actions, |_| {
            let text = texts[index.get()].clone();
            index.set(index.get() + 1);
            async move { Ok(Reply::Text(text)) }
        })
        .await
    }

    fn with_id(id: &str, text: &str) -> Recalled {
        Recalled {
            id: Some(id.to_string()),
            text: text.to_string(),
        }
    }

    #[test]
    fn two_records_with_the_same_text_and_different_ids_are_both_kept() {
        let mut budget = RecallBudget::default();
        budget.add_records(vec![with_id("a", "[t] same"), with_id("b", "[t] same")]);
        budget.add_records(vec![with_id("a", "[t] same")]);
        assert_eq!(budget.kept(), ["[t] same", "[t] same"]);
        assert_eq!(budget.duplicates, 1);
        assert_eq!(budget.kept_keys(), ["a", "b"]);
    }

    #[test]
    fn a_ledger_of_text_hashes_dedups_once_then_moves_to_ids() {
        // A ledger from before #3369 holds the text hash of a record that it sent.
        let old = BTreeSet::from([record_hash("[t] sent before")]);
        let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, old.clone());
        budget.add_records(vec![
            with_id("old-id", "[t] sent before"),
            with_id("new", "[t] new"),
        ]);
        assert_eq!(budget.kept(), ["[t] new"]);
        assert_eq!(budget.already_sent, 1);
        // The ledger learns the id of the old record beside the kept one.
        let keys = budget.kept_keys();
        assert_eq!(keys, ["new", "old-id"]);
        // Next turn the ledger holds ids, so the old record is skipped by its id.
        let mut sent = old;
        sent.extend(keys);
        let mut next = RecallBudget::new(RECALL_BUDGET_BYTES, sent);
        next.add_records(vec![with_id("old-id", "[t] sent before")]);
        assert!(next.kept().is_empty());
        assert!(
            next.kept_keys().is_empty(),
            "an id match is not migrated again"
        );
    }

    #[test]
    fn a_record_with_no_id_is_keyed_by_its_text_hash() {
        let mut budget = RecallBudget::new(
            RECALL_BUDGET_BYTES,
            BTreeSet::from([record_hash("[t] old")]),
        );
        budget.add_text("[t] old\n[t] new\n[t] new");
        assert_eq!(budget.kept(), ["[t] new"]);
        assert_eq!(budget.duplicates, 1);
        assert_eq!(budget.kept_keys(), [record_hash("[t] new")]);
    }

    #[test]
    fn hook_text_keeps_the_omission_notice_out_of_memory() {
        // The notice is llmenv text, so it must not sit under the ICM memory header (#2764).
        let mut budget = RecallBudget::default();
        budget.add_strings(vec!["x".repeat(RECALL_BUDGET_BYTES + 1)]);
        let text = budget.hook_text(Vec::new());
        assert!(!text.memory.contains("omitted"), "{}", text.memory);
        assert!(
            text.notes.contains("1 lower-priority memories omitted"),
            "{}",
            text.notes
        );
    }

    #[tokio::test]
    async fn budget_keeps_whole_records_and_reports_the_omitted_count() {
        let (text, budget) = run_texts(vec![records(30, 500, "a")]).await;
        let notice = budget.notice().expect("has a notice");
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
        let actions = vec![
            Action::Recall(String::new()),
            Action::Recall(String::new()),
            Action::Recall(String::new()),
        ];
        let (text, budget) = run_with_budget(actions, |_| {
            calls.set(calls.get() + 1);
            async {
                Ok(Reply::Text(format!(
                    "[t] {}\n[t] {}",
                    "a".repeat(7_900),
                    "b".repeat(500)
                )))
            }
        })
        .await;
        assert_eq!(calls.get(), 1, "budget was full after the first recall");
        assert_eq!(budget.skipped_actions, 2);
        assert_eq!(budget.omitted, 1);
        assert!(
            budget.notice().unwrap_or_default().ends_with(&format!(
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
        let (text, budget) = run_with_budget(
            vec![Action::WakeUp(
                crate::hook_run::action::WakeUpArgs::default(),
            )],
            |_| {
                let big = big.clone();
                async move { Ok(Reply::Text(big)) }
            },
        )
        .await;
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
        budget.add_strings(vec![record_of(3_000), format!("{}y", record_of(2_999))]);
        budget.add_strings(vec![record_of(1_995), record_of(1_994)]);
        let lengths: Vec<usize> = budget.kept().iter().map(|r| r.len()).collect();
        assert_eq!(lengths, [3_000, 3_000, 1_994]);
        assert_eq!(budget.omitted, 1);
    }

    #[test]
    fn a_full_budget_starts_at_exactly_the_minimum_free_bytes() {
        let mut budget = RecallBudget::default();
        // 7,798 + 2 separator bytes leaves exactly MIN_FREE_BYTES free: not full yet.
        budget.add_strings(vec![record_of(
            RECALL_BUDGET_BYTES - MIN_FREE_BYTES - SEPARATOR_BYTES,
        )]);
        assert!(!budget.is_full());
        budget.add_strings(vec![record_of(6)]);
        assert!(budget.is_full());
    }

    #[test]
    fn sent_records_are_skipped_and_counted() {
        let sent: BTreeSet<String> = [record_hash("[t] old")].into();
        let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, sent);
        budget.add_text("[t] old\n[t] new");
        assert_eq!(budget.kept(), ["[t] new"]);
        assert_eq!(budget.already_sent, 1);
        assert_eq!(budget.kept_keys(), [record_hash("[t] new")]);
    }

    #[test]
    fn a_smaller_limit_is_respected() {
        let mut budget = RecallBudget::new(2_000, BTreeSet::new());
        budget.add_strings(vec![record_of(1_500), format!("{}y", record_of(1_499))]);
        assert_eq!(budget.kept().len(), 1);
        assert_eq!(budget.omitted, 1);
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
        budget.add_strings(vec![
            "[t] a".to_string(),
            "[t] a".to_string(),
            "[t] bb".to_string(),
        ]);
        assert_eq!(
            budget.trace_line(true).as_deref(),
            Some(
                "[LLMENV_CONTEXT] recall_entries=3 recall_bytes=16 injected_entries=2 \
                     injected_bytes=11 duplicate_records=1 already_sent=0 omitted=0 \
                     skipped_actions=0"
            )
        );
    }

    // The line must keep the shape that website/docs/trace.md shows (#3018).
    #[test]
    fn trace_line_matches_the_docs_page() {
        let mut budget = RecallBudget::default();
        budget.add_strings(vec!["[t] a".to_string()]);
        let line = budget.trace_line(true).unwrap_or_default();
        assert!(
            crate::trace::docs_sync::docs_show(&line),
            "website/docs/trace.md shows no template for `{line}`"
        );
    }

    #[tokio::test]
    async fn passthrough_text_skips_empty_results_and_exact_duplicates() {
        let texts = ["", "pack", "pack", "other"];
        let index = Cell::new(0);
        let (text, _) = run_with_budget(
            vec![Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()); 4],
            |_| {
                let text = texts[index.get()].to_string();
                index.set(index.get() + 1);
                async move { Ok(Reply::Text(text)) }
            },
        )
        .await;
        assert_eq!(text, "pack\n\nother");
    }

    #[tokio::test]
    async fn a_failing_action_keeps_the_text_of_the_others() {
        let actions = vec![Action::Recall(String::new()), Action::Recall(String::new())];
        let mut calls = 0;
        let (text, _) = run_with_budget(actions, |_| {
            calls += 1;
            let first = calls == 1;
            async move {
                if first {
                    Err(anyhow::anyhow!("boom"))
                } else {
                    Ok(Reply::Text("kept record".to_string()))
                }
            }
        })
        .await;
        assert_eq!(text, "kept record");
    }

    /// Run one passthrough action per outcome. `None` is a failed action.
    fn run_outcomes(outcomes: Vec<Option<String>>) -> String {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let actions =
            vec![Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()); outcomes.len()];
        let mut outcomes = outcomes.into_iter();
        let (text, _) = runtime.block_on(run_with_budget(actions, |_| {
            let outcome = outcomes.next().expect("one outcome per action");
            async move {
                outcome
                    .map(Reply::Text)
                    .ok_or_else(|| anyhow::anyhow!("boom"))
            }
        }));
        text
    }

    proptest! {
        /// A failed action changes nothing else: the text equals the run without that action.
        #[test]
        fn failing_actions_do_not_change_the_text_of_the_others(
            outcomes in prop::collection::vec(prop::option::of("[a-z]{1,8}"), 1..10),
        ) {
            let kept: Vec<Option<String>> = outcomes.iter().filter(|o| o.is_some()).cloned().collect();
            prop_assert_eq!(run_outcomes(outcomes), run_outcomes(kept));
        }
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
            budget.add_strings(input.clone());
            let kept_bytes: usize = budget.kept().iter().map(|r| r.len() + SEPARATOR_BYTES).sum();
            prop_assert!(kept_bytes <= RECALL_BUDGET_BYTES);
            for record in budget.kept() {
                prop_assert!(input.iter().any(|i| i == record));
            }
            prop_assert_eq!(
                budget.kept().len() + budget.omitted + budget.duplicates,
                input.len()
            );
        }
    }
}
