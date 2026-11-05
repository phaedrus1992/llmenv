//! What a set of recall calls returned, and how many failed (#2897).

use crate::agent_notice;
use crate::hook_run::action::{Recalled, Reply};

/// What a set of recall calls returned.
#[derive(Debug, Default)]
pub(super) struct Outcome {
    pub(super) records: Vec<Recalled>,
    pub(super) calls: usize,
    pub(super) failed: usize,
}

impl Outcome {
    /// Record one call. `None` means no call ran, so nothing is counted.
    pub(super) fn push(&mut self, result: Option<anyhow::Result<Reply>>) {
        let Some(result) = result else {
            return;
        };
        self.calls += 1;
        match result.and_then(Reply::into_records) {
            Ok(records) => self.records.extend(records),
            Err(e) => {
                self.failed += 1;
                tracing::error!("adaptive recall call failed, its records are skipped: {e:#}");
                agent_notice::record(format!(
                    "llmenv: a memory recall call failed, so its records were not sent: {e:#}. \
                     Fix: run `llmenv doctor` to check the memory server."
                ));
            }
        }
    }

    pub(super) fn merge(&mut self, other: Outcome) {
        self.records.extend(other.records);
        self.calls += other.calls;
        self.failed += other.failed;
    }

    /// The records as text, one per line.
    #[cfg(test)]
    pub(super) fn text(&self) -> String {
        self.records
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn all_failed(&self) -> bool {
        self.calls > 0 && self.failed == self.calls
    }
}
