//! The JSON form of an `icm_memory_recall` reply (icm 0.11.5 and later, #3368).
//!
//! The text form changes its layout between icm releases, and it has no record links. The JSON
//! form gives each record its id, so a reader does not have to parse free text.

use serde::Deserialize;

mod error;
pub(crate) use error::RecallJsonError;

/// The value of the recall `format` argument that asks icm for the JSON form.
pub(crate) const FORMAT: &str = "json";

/// One record of a JSON recall reply. icm adds more fields, such as `score`, `bm25`, and
/// `raw_excerpt`; a reader that does not need them ignores them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct RecallRecord {
    /// The icm record id. It is never empty.
    pub id: String,
    pub topic: String,
    pub summary: String,
    /// The icm importance level, such as `high`, as icm spells it.
    pub importance: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub related_ids: Vec<String>,
}

/// Most characters of a bad reply that an error quotes, so a large reply does not flood a log.
const QUOTE_CHARS: usize = 120;

fn quote(text: &str) -> String {
    text.chars().take(QUOTE_CHARS).collect()
}

/// The records of a JSON recall reply, in icm's rank order.
///
/// icm can append an advisory line after the list, so only the first JSON value is read.
///
/// # Errors
/// The reply does not start with a JSON array, or one element is not a record with a non-empty
/// `id`, a `topic`, a `summary`, and an `importance`.
pub(crate) fn parse(body: &str) -> Result<Vec<RecallRecord>, RecallJsonError> {
    let body = body.trim_start();
    let first = serde_json::Deserializer::from_str(body)
        .into_iter::<Vec<serde_json::Value>>()
        .next();
    let values = match first {
        Some(Ok(values)) => values,
        Some(Err(e)) => return Err(not_a_list(&e.to_string(), body)),
        None => return Err(not_a_list("the reply is empty", body)),
    };
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| record(index, value))
        .collect()
}

fn not_a_list(reason: &str, body: &str) -> RecallJsonError {
    RecallJsonError::NotAList {
        reason: reason.to_string(),
        start: quote(body),
    }
}

fn record(index: usize, value: serde_json::Value) -> Result<RecallRecord, RecallJsonError> {
    let bad = |reason: String, value: &serde_json::Value| RecallJsonError::BadRecord {
        index,
        reason,
        record: quote(&value.to_string()),
    };
    let parsed = RecallRecord::deserialize(&value).map_err(|e| bad(e.to_string(), &value))?;
    if parsed.id.trim().is_empty() {
        return Err(bad("the id is empty".to_string(), &value));
    }
    Ok(parsed)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A reply that icm 0.11.5 gave for a two-record recall, with the advisory line it can add.
    const CAPTURED: &str = r#"[{"id":"01JB7Q","topic":"decisions-llmenv","summary":"Use JSON recall","importance":"high","weight":0.95,"keywords":["icm","json"],"related_ids":["01JB7R"],"created_at":"2026-10-22T10:00:00+00:00","updated_at":"2026-10-22T10:00:00+00:00","score":0.912,"bm25":4.2},{"id":"01JB7R","topic":"context-llmenv","summary":"Prune reads importance","importance":"low","weight":0.4,"keywords":[],"related_ids":[],"created_at":"2026-10-21T10:00:00+00:00","updated_at":"2026-10-21T10:00:00+00:00"}]
[icm: 2 memories]"#;

    #[test]
    fn parse_reads_a_captured_reply_in_rank_order() {
        let records = parse(CAPTURED).unwrap();
        assert_eq!(
            records,
            vec![
                RecallRecord {
                    id: "01JB7Q".into(),
                    topic: "decisions-llmenv".into(),
                    summary: "Use JSON recall".into(),
                    importance: "high".into(),
                    keywords: vec!["icm".into(), "json".into()],
                    related_ids: vec!["01JB7R".into()],
                },
                RecallRecord {
                    id: "01JB7R".into(),
                    topic: "context-llmenv".into(),
                    summary: "Prune reads importance".into(),
                    importance: "low".into(),
                    keywords: vec![],
                    related_ids: vec![],
                },
            ]
        );
    }

    #[test]
    fn parse_reads_an_empty_list_as_no_records() {
        assert_eq!(parse("[]"), Ok(vec![]));
        assert_eq!(parse("  []\n"), Ok(vec![]));
    }

    #[test]
    fn parse_refuses_a_text_reply_and_names_the_fix() {
        let err = parse("--- 01JB7Q ---\n  topic: t\n").unwrap_err();
        assert!(matches!(err, RecallJsonError::NotAList { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("starts with \"--- 01JB7Q ---"), "{text}");
        assert!(
            text.contains("Fix: upgrade icm to 0.11.5 or later"),
            "{text}"
        );
    }

    #[test]
    fn parse_refuses_an_empty_reply() {
        let err = parse("  ").unwrap_err();
        assert_eq!(
            err,
            RecallJsonError::NotAList {
                reason: "the reply is empty".into(),
                start: String::new(),
            }
        );
    }

    #[test]
    fn parse_names_the_record_that_has_no_id() {
        let err = parse(r#"[{"topic":"t","summary":"s","importance":"low"}]"#).unwrap_err();
        let RecallJsonError::BadRecord { index, reason, .. } = err else {
            panic!("want BadRecord, got {err:?}");
        };
        assert_eq!(index, 0);
        assert_eq!(reason, "missing field `id`");
    }

    #[test]
    fn parse_refuses_an_empty_id_and_a_record_that_is_not_an_object() {
        let empty = parse(r#"[{"id":" ","topic":"t","summary":"s","importance":"low"}]"#);
        assert!(
            matches!(&empty, Err(RecallJsonError::BadRecord { reason, .. }) if reason == "the id is empty"),
            "{empty:?}"
        );
        let good = r#"{"id":"a","topic":"t","summary":"s","importance":"low"}"#;
        let err = parse(&format!("[{good}, 7]")).unwrap_err();
        assert!(
            matches!(err, RecallJsonError::BadRecord { index: 1, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_bad_reply_is_quoted_short() {
        let long = format!("x{}", "y".repeat(500));
        let RecallJsonError::NotAList { start, .. } = parse(&long).unwrap_err() else {
            panic!("want NotAList");
        };
        assert_eq!(start.chars().count(), QUOTE_CHARS);
    }

    proptest! {
        /// Any list of well-formed records reads back as the same records, whatever follows it.
        #[test]
        fn parse_round_trips_records(
            ids in proptest::collection::vec("[a-zA-Z0-9]{1,12}", 0..6),
            summary in "\\PC{0,40}",
            tail in "(\n\\[icm: [a-z ]{0,20}\\])?",
        ) {
            let values: Vec<serde_json::Value> = ids
                .iter()
                .map(|id| serde_json::json!({
                    "id": id, "topic": "t", "summary": summary, "importance": "medium",
                    "weight": 0.5, "score": 0.1,
                }))
                .collect();
            let body = format!("{}{tail}", serde_json::Value::Array(values));
            let got: Vec<String> = parse(&body).unwrap().into_iter().map(|r| r.id).collect();
            prop_assert_eq!(got, ids);
        }
    }
}
