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
        self.add_records(test_records(text));
    }

    /// The keys to add to the ledger's sent set: each kept record, and each record that matched
    /// the ledger by its old text hash only. A ledger from before #3369 then moves to ids.
    pub(super) fn kept_keys(&self) -> Vec<String> {
        self.kept
            .iter()
            .map(|r| r.id.clone())
            .chain(self.migrated.iter().cloned())
            .collect()
    }

    /// Whether the context already holds `record`. A record also matches the text hash that a
    /// ledger from before #3369 holds; such a match is noted, so the ledger learns the id.
    fn already_sent(&mut self, record: &Recalled) -> bool {
        if self.sent.contains(&record.id) {
            return true;
        }
        if self.sent.contains(&record_hash(&record.text)) {
            self.migrated.push(record.id.clone());
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
            } else if self.seen.contains(&record.id) {
                self.duplicates += 1;
            } else if self.kept_bytes + bytes + SEPARATOR_BYTES <= self.limit {
                self.kept_bytes += bytes + SEPARATOR_BYTES;
                self.seen.insert(record.id.clone());
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
                .map(|text| Recalled::from_text(&text))
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
        let answered = run(action).await.and_then(|reply| {
            if recall {
                reply.into_records().map(Reply::Records)
            } else {
                Ok(reply)
            }
        });
        let Ok(reply) = answered.inspect_err(|e| {
            tracing::error!("memory action failed, its text is missing: {e:#}");
            crate::agent_notice::record(format!(
                "llmenv could not run a memory action, so its text is missing: {e:#}. \
                 Fix: run `llmenv doctor` to check the memory server."
            ));
        }) else {
            continue;
        };
        match reply {
            Reply::Records(records) => budget.add_records(records),
            Reply::Text(text) => {
                if !text.is_empty() && !passthrough.contains(&text) {
                    passthrough.push(text);
                }
            }
        }
    }
    let text = budget.render(passthrough);
    (text, budget)
}

/// One record for each `[<topic>] ` record of `text`, with the text hash as the id.
#[cfg(test)]
fn test_records(text: &str) -> Vec<Recalled> {
    crate::hook_run::action::split_recall_records(text)
        .iter()
        .map(|t| Recalled::from_text(t))
        .collect()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests;
