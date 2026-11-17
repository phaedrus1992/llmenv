use super::{
    HookEvent, HookText, deny_drops_memory, join_notes, join_precomputed,
    recover_from_pipeline_error,
};
use proptest::strategy::Strategy as _;

fn failure() -> anyhow::Error {
    anyhow::anyhow!("tag `bad tag` is not valid")
}

#[test]
fn a_failed_pipeline_keeps_a_computed_nudge() {
    // #2730: the nudge is recorded as delivered when it is computed, so it must reach the agent.
    let text = recover_from_pipeline_error(
        HookEvent::PostToolUse,
        failure(),
        [None, None, Some("nudge: wait for the user".to_string())],
    )
    .expect("a computed nudge must survive the pipeline error");
    assert_eq!(
        text,
        HookText::from_notes("nudge: wait for the user".to_string())
    );
    let notice = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notice.contains("llmenv: memory") && notice.contains("bad tag"),
        "the skipped memory step must reach the agent: {notice}"
    );
    assert!(
        notice.contains("memory PostToolUse skipped") || notice.contains("skipped: tag"),
        "the skipped memory step must reach the agent: {notice}"
    );
}

#[test]
fn a_failed_pipeline_with_no_computed_text_returns_the_error() {
    let err = recover_from_pipeline_error(HookEvent::PostToolUse, failure(), [None, None, None])
        .expect_err("nothing to keep, so the error must propagate");
    assert!(err.to_string().contains("bad tag"), "got: {err}");
}

#[test]
fn a_deny_drops_only_memory_that_was_computed() {
    let with_memory = HookText {
        memory: "recalled".to_string(),
        notes: "__DENY__:why".to_string(),
    };
    assert!(deny_drops_memory(&with_memory));
    assert!(!deny_drops_memory(&HookText::from_notes(
        "__DENY__:why".to_string()
    )));
    let allowed = HookText {
        memory: "recalled".to_string(),
        notes: "a note".to_string(),
    };
    assert!(
        !deny_drops_memory(&allowed),
        "no deny, so nothing is dropped"
    );
}

#[test]
fn join_notes_joins_only_the_parts_that_have_text() {
    assert_eq!(join_notes(Some("notice"), "note"), "notice\n\nnote");
    assert_eq!(join_notes(Some("notice"), ""), "notice");
    assert_eq!(join_notes(Some(""), "note"), "note");
    assert_eq!(join_notes(None, "note"), "note");
    assert_eq!(join_notes(None, ""), "");
}

#[test]
fn precomputed_texts_join_in_success_path_order() {
    let joined = join_precomputed([
        Some("pre".to_string()),
        Some("turn".to_string()),
        Some("nudge".to_string()),
    ]);
    assert_eq!(joined.as_deref(), Some("pre\nturn\nnudge"));
}

#[test]
fn an_empty_precomputed_text_adds_no_separator() {
    let joined = join_precomputed([Some(String::new()), None, Some("nudge".to_string())]);
    assert_eq!(joined.as_deref(), Some("nudge"));
}

#[test]
fn a_deny_precomputed_text_keeps_its_prefix() {
    // Matches the success path: the deny marker stays first, and later text appends after it.
    let joined = join_precomputed([
        Some("__DENY__:already read".to_string()),
        Some("turn".to_string()),
        None,
    ]);
    assert_eq!(joined.as_deref(), Some("__DENY__:already read\nturn"));
}

/// A slot value: absent, empty, plain text, or a deny marker.
fn slot() -> impl proptest::strategy::Strategy<Value = Option<String>> {
    proptest::prop_oneof![
        proptest::strategy::Just(None),
        proptest::strategy::Just(Some(String::new())),
        "[a-z]{1,8}".prop_map(Some),
        "[a-z]{1,8}".prop_map(|t| Some(format!("__DENY__:{t}"))),
    ]
}

proptest::proptest! {
    #[test]
    fn joined_output_follows_the_success_path_rules(a in slot(), b in slot(), c in slot()) {
        let slots = [a, b, c];
        let joined = join_precomputed(slots.clone());
        proptest::prop_assert_eq!(joined.is_none(), slots.iter().all(Option::is_none));
        let texts: Vec<&str> = slots
            .iter()
            .flatten()
            .map(String::as_str)
            .filter(|t| !t.is_empty())
            .collect();
        let joined = joined.unwrap_or_default();
        match texts.iter().rposition(|t| t.starts_with("__DENY__:")) {
            // The last deny replaces what came before it; later texts append after it.
            Some(last_deny) => proptest::prop_assert_eq!(joined, texts[last_deny..].join("\n")),
            None => proptest::prop_assert_eq!(joined, texts.join("\n")),
        }
    }
}
