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
            },
            RecallRecord {
                id: "01JB7R".into(),
                topic: "context-llmenv".into(),
                summary: "Prune reads importance".into(),
                importance: "low".into(),
                keywords: vec![],
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
fn parse_refuses_a_text_reply_and_names_the_upgrade() {
    let err = parse("--- 01JB7Q ---\n  topic: t\n").unwrap_err();
    assert!(matches!(err, RecallJsonError::NotAList { .. }), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains("starts with \"--- 01JB7Q ---   topic: \""),
        "{text}"
    );
    assert!(
        text.contains("Fix: upgrade icm to 0.11.5 or later"),
        "{text}"
    );
}

#[test]
fn an_empty_reply_names_the_server_check_not_an_upgrade() {
    let err = parse("  ").unwrap_err();
    assert_eq!(err, RecallJsonError::Empty);
    assert!(err.to_string().contains("llmenv doctor"), "{err}");
    assert!(!err.to_string().contains("upgrade"), "{err}");
}

#[test]
fn a_cut_off_list_is_named_as_cut_off() {
    let err = parse(r#"[{"id":"a","topic":"t","summ"#).unwrap_err();
    let RecallJsonError::CutOff { bytes, .. } = err else {
        unreachable!("want CutOff, got {err:?}");
    };
    assert_eq!(bytes, 28);
}

#[test]
fn parse_names_the_record_that_has_no_id_by_its_fields() {
    let err = parse(r#"[{"topic":"t","summary":"secret text","importance":"low"}]"#).unwrap_err();
    assert_eq!(
        err,
        RecallJsonError::BadRecord {
            index: 0,
            reason: "missing field `id`".into(),
            shape: "an object with the fields [importance, summary, topic]".into(),
        }
    );
    assert!(!err.to_string().contains("secret text"), "{err}");
}

#[test]
fn parse_refuses_a_record_that_is_not_an_object() {
    let good = r#"{"id":"a","topic":"t","summary":"s","importance":"low"}"#;
    let err = parse(&format!("[{good}, 7]")).unwrap_err();
    assert!(
        matches!(&err, RecallJsonError::BadRecord { index: 1, shape, .. } if shape == "a number"),
        "{err:?}"
    );
}

#[test]
fn a_bad_reply_is_quoted_short_and_on_one_line() {
    let long = format!("x\n{}", "y".repeat(500));
    let RecallJsonError::NotAList { start, .. } = parse(&long).unwrap_err() else {
        unreachable!("want NotAList");
    };
    assert_eq!(start, format!("x {}", "y".repeat(QUOTE_CHARS - 2)));
}

fn arb_record() -> impl Strategy<Value = RecallRecord> {
    (
        "[a-zA-Z0-9]{1,12}",
        "\\PC{0,20}",
        "\\PC{0,40}",
        prop::sample::select(vec!["critical", "high", "medium", "low"]),
        prop::collection::vec("\\PC{0,8}", 0..3),
    )
        .prop_map(|(id, topic, summary, importance, keywords)| RecallRecord {
            id,
            topic,
            summary,
            importance: importance.to_string(),
            keywords,
        })
}

proptest! {
    /// Any list of records reads back field for field, whatever follows the list.
    #[test]
    fn parse_round_trips_every_field(
        records in prop::collection::vec(arb_record(), 0..6),
        tail in "(\n\\[icm: [a-z ]{0,20}\\])?",
    ) {
        let values: Vec<serde_json::Value> = records
            .iter()
            .map(|r| serde_json::json!({
                "id": r.id, "topic": r.topic, "summary": r.summary,
                "importance": r.importance, "keywords": r.keywords,
                "weight": 0.5, "score": 0.1, "related_ids": ["x"],
            }))
            .collect();
        let body = format!("{}{tail}", serde_json::Value::Array(values));
        prop_assert_eq!(parse(&body).unwrap(), records);
    }

    /// A blank id is refused at its index.
    #[test]
    fn a_blank_id_is_refused_at_its_index(blank in "[ \t\n]{0,4}", before in 0usize..3) {
        let good = serde_json::json!({"id": "a", "topic": "t", "summary": "s", "importance": "low"});
        let mut values = vec![good; before];
        values.push(serde_json::json!({"id": blank, "topic": "t", "summary": "s", "importance": "low"}));
        let err = parse(&serde_json::Value::Array(values).to_string()).unwrap_err();
        prop_assert!(
            matches!(&err, RecallJsonError::BadRecord { index, .. } if *index == before),
            "{:?}", err
        );
    }

    /// Any text reads or fails, and a failure never quotes more than the quote limit.
    #[test]
    fn parse_never_panics_and_quotes_little(body in "\\PC{0,300}") {
        if let Err(RecallJsonError::NotAList { start, .. }) = parse(&body) {
            prop_assert!(start.chars().count() <= QUOTE_CHARS);
            prop_assert!(!start.contains('\n'));
        }
    }
}
