//! What one memory action answers: plain text, or the records of a JSON recall (#3369).

pub(crate) use super::recalled::Recalled;

/// What one memory action answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// The text of a call that is not a recall, such as the wake-up pack.
    Text(String),
    /// The records of a recall, in icm's rank order.
    Records(Vec<Recalled>),
}

impl Reply {
    /// The reply as text: the records one per line.
    pub(crate) fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Records(records) => records
                .into_iter()
                .map(|r| r.text)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// The reply as records. Text is split at its `[<topic>] ` lines, with no ids.
    pub(crate) fn into_records(self) -> Vec<Recalled> {
        match self {
            Self::Records(records) => records,
            Self::Text(text) => super::split_recall_records(&text)
                .into_iter()
                .map(|text| Recalled { id: None, text })
                .collect(),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::memory::recall_json::RecallRecord;

    fn record(id: &str, topic: &str, summary: &str) -> RecallRecord {
        let body = serde_json::json!([{
            "id": id, "topic": topic, "summary": summary, "importance": "low",
        }]);
        crate::memory::recall_json::parse(&body.to_string())
            .unwrap()
            .remove(0)
    }

    #[test]
    fn a_record_renders_as_one_compact_line_with_its_id() {
        let r = Recalled::from_record(&record("01A", "decisions-x", "use json"));
        assert_eq!(r.id.as_deref(), Some("01A"));
        assert_eq!(r.text, "[decisions-x] use json");
        assert_eq!(r.topic(), Some("decisions-x"));
        assert_eq!(r.key(), "01A");
    }

    #[test]
    fn a_newline_in_a_summary_cannot_forge_a_record() {
        let r = Recalled::from_record(&record("01A", "t", "one\n[evil] two\r"));
        assert_eq!(r.text, "[t] one [evil] two ");
        let records = Reply::Records(vec![r]).into_text();
        assert_eq!(super::super::split_recall_records(&records).len(), 1);
    }

    #[test]
    fn a_text_record_is_keyed_by_its_hash() {
        let records = Reply::Text("[t] a\n[t] b".into()).into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, None);
        assert_eq!(
            records[0].key(),
            crate::hook_run::session_ledger::record_hash("[t] a")
        );
    }

    proptest! {
        /// Records joined as text split back into the same records.
        #[test]
        fn records_round_trip_through_text(
            rows in proptest::collection::vec(("[a-z]{1,8}", "[a-z \\n]{0,20}"), 0..6),
        ) {
            let records: Vec<Recalled> = rows
                .iter()
                .enumerate()
                .map(|(i, (t, s))| Recalled::from_record(&record(&format!("id{i}"), t, s)))
                .collect();
            let texts: Vec<String> = records.iter().map(|r| r.text.trim_end().to_string()).collect();
            let back: Vec<String> = Reply::Text(Reply::Records(records).into_text())
                .into_records()
                .into_iter()
                .map(|r| r.text)
                .collect();
            prop_assert_eq!(back, texts);
        }
    }
}
