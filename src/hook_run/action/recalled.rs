//! One recalled memory as the hook injects it (#3369).

use crate::memory::recall_json::RecallRecord;

/// One recalled memory as the hook injects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recalled {
    /// The icm record id, the key of the sent set.
    pub(crate) id: String,
    /// The icm topic as stored. Scope checks read this, not the rendered text.
    pub(crate) topic: String,
    /// The record as one `[<topic>] <summary>` line, the compact form of an icm recall.
    pub(crate) text: String,
}

impl Recalled {
    /// The injected form of an icm record. The summary and the topic are put on one line, and
    /// `]` leaves the topic, because a stored record could otherwise forge a `[topic]` record.
    pub(crate) fn from_record(record: &RecallRecord) -> Self {
        let summary = record.summary.replace(['\n', '\r'], " ");
        let topic = record.topic.replace(['\n', '\r', ']'], " ");
        Self {
            id: record.id.clone(),
            topic: record.topic.clone(),
            text: format!("[{topic}] {summary}"),
        }
    }

    /// A record whose id is the hash of its text, as test fixtures need.
    #[cfg(test)]
    pub(crate) fn from_text(text: &str) -> Self {
        let topic = crate::hook_run::relevance::record_topic(text).unwrap_or_default();
        Self {
            id: crate::hook_run::session_ledger::record_hash(text),
            topic: topic.to_string(),
            text: text.to_string(),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn record(id: &str, topic: &str, summary: &str) -> RecallRecord {
        let body = serde_json::json!([{
            "id": id, "topic": topic, "summary": summary, "importance": "low",
        }]);
        crate::memory::recall_json::parse(&body.to_string())
            .unwrap()
            .remove(0)
    }

    #[test]
    fn a_record_renders_as_one_compact_line_with_its_id_and_raw_topic() {
        let r = Recalled::from_record(&record("01A", "web-fetch]x", "use json"));
        assert_eq!(r.id, "01A");
        assert_eq!(r.topic, "web-fetch]x");
        assert_eq!(r.text, "[web-fetch x] use json");
    }

    #[test]
    fn a_newline_in_a_summary_cannot_forge_a_record() {
        let r = Recalled::from_record(&record("01A", "t", "one\n[evil] two\r"));
        assert_eq!(r.text, "[t] one [evil] two ");
    }

    proptest! {
        /// Any stored topic and summary render as exactly one record that keeps the topic.
        #[test]
        fn any_record_renders_as_one_record(topic in "\\PC{0,30}", summary in "\\PC{0,60}") {
            let topic = format!("t{topic}");
            let r = Recalled::from_record(&record("id", &topic, &summary));
            prop_assert!(!r.text.contains('\n') && !r.text.contains('\r'));
            let split = crate::hook_run::action::split_recall_records(&r.text);
            prop_assert!(split.len() <= 1, "{:?}", split);
            prop_assert_eq!(r.topic, topic);
        }
    }
}
