//! One recalled memory as the hook injects it (#3369).

use crate::memory::recall_json::RecallRecord;

/// One recalled memory as the hook injects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recalled {
    /// The icm record id. `None` for text that did not come from a JSON recall, such as a
    /// scope note; such a record is keyed by the hash of its text.
    pub(crate) id: Option<String>,
    /// The record as one `[<topic>] <summary>` line, the compact form of an icm recall.
    pub(crate) text: String,
}

impl Recalled {
    /// The injected form of an icm record. The summary is put on one line, because a newline
    /// in a stored summary could otherwise start a forged `[topic]` record.
    pub(crate) fn from_record(record: &RecallRecord) -> Self {
        let summary = record.summary.replace(['\n', '\r'], " ");
        let topic = record.topic.replace(['\n', '\r', ']'], " ");
        Self {
            id: Some(record.id.clone()),
            text: format!("[{topic}] {summary}"),
        }
    }

    /// The key of the sent set: the icm id, or the text hash for a record with no id.
    pub(crate) fn key(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| crate::hook_run::session_ledger::record_hash(&self.text))
    }

    /// The topic of the record, from its `[<topic>]` prefix.
    pub(crate) fn topic(&self) -> Option<&str> {
        crate::hook_run::relevance::record_topic(&self.text)
    }
}
