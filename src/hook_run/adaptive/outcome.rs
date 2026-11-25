//! What a set of recall calls returned, and how many failed (#2897).

use crate::agent_notice;
use crate::hook_run::action::{Recalled, Reply};
use crate::hook_run::recall_breaker;
use crate::hook_run::recall_deadline::Cut;

/// What a set of recall calls returned.
#[derive(Debug, Default)]
pub(super) struct Outcome {
    pub(super) records: Vec<Recalled>,
    pub(super) calls: usize,
    pub(super) failed: usize,
    /// A call was cut off by the time budget. The server did not fail, and the answer is unknown.
    pub(super) cut: bool,
}

impl Outcome {
    /// Record one call. `None` means no call ran, so nothing is counted.
    pub(super) fn push(&mut self, result: Option<anyhow::Result<Reply>>) {
        let Some(result) = result else {
            return;
        };
        // A call the open breaker skipped never ran, so it is neither a success nor a failure.
        // The breaker's own notice names the skip, in the process that opened it.
        if matches!(&result, Err(e) if e.is::<recall_breaker::Skipped>()) {
            return;
        }
        // A call that the time budget cut off is neither a success nor a failure of the server.
        // One notice names the budget, and a repeat of the same notice is not recorded again.
        if let Err(e) = &result
            && let Some(cut) = e.downcast_ref::<Cut>()
        {
            self.cut = true;
            agent_notice::record(cut.notice());
            return;
        }
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
        self.cut |= other.cut;
    }

    /// Every call answered: none failed and the budget cut none off.
    pub(super) fn complete(&self) -> bool {
        self.failed == 0 && !self.cut
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_call_is_neither_a_failure_nor_a_record_and_the_set_is_not_complete() {
        let cut: anyhow::Result<Reply> =
            Err(crate::hook_run::recall_deadline::Deadline::turn_start()
                .cut_error()
                .into());
        let mut out = Outcome::default();
        out.push(Some(cut));
        assert_eq!((out.calls, out.failed), (0, 0));
        assert!(out.records.is_empty() && out.cut && !out.complete());
        assert!(!out.all_failed());
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(text.contains("turn start recall budget of 3 s"), "{text}");
        assert!(Outcome::default().complete());
    }

    #[test]
    fn a_skipped_call_is_neither_a_failure_nor_a_record() {
        let skipped: anyhow::Result<Reply> = Err(recall_breaker::Skipped.into());
        let mut out = Outcome::default();
        out.push(Some(skipped));
        assert_eq!((out.calls, out.failed), (0, 0));
        assert!(out.records.is_empty());
        assert!(!out.all_failed());
    }
}
