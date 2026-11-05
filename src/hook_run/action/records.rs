//! The records of an ICM recall text, and the records that a recall across all projects
//! leaves out.

/// The topic of the scope-context record that SessionEnd stores. The topic names no project, so
/// only a recall across all projects returns it.
pub(super) const SCOPE_CONTEXT_TOPIC: &str = "llmenv-scope-context";

/// Whether `record` belongs to one project but carries no project in its topic: a fetched page
/// (#2911), or a scope-context record, which describes the tags and bundles of the project that
/// stored it (#2938). A recall across all projects leaves such records out.
pub(super) fn is_unscoped(record: &super::Recalled) -> bool {
    record.topic().is_some_and(|topic| {
        topic == SCOPE_CONTEXT_TOPIC || crate::hook_run::web_fetch::is_web_fetch_topic(topic)
    })
}

/// Longest topic, in characters, that a record's first line may carry.
pub(super) const MAX_TOPIC_CHARS: usize = 200;

/// Whether `line` opens an ICM recall record: `[<topic>] <text>`.
///
/// Phases: the line starts with `[`; the first `]` ends the topic; the topic has 1 to
/// [`MAX_TOPIC_CHARS`] characters; a space follows the `]`.
pub(super) fn is_record_start(line: &str) -> bool {
    let Some(after_open) = line.strip_prefix('[') else {
        return false;
    };
    let Some((topic, after_topic)) = after_open.split_once(']') else {
        return false;
    };
    (1..=MAX_TOPIC_CHARS).contains(&topic.chars().count()) && after_topic.starts_with(' ')
}

/// Split one recall action's text into ICM records, so a byte budget can keep
/// whole records and drop the rest (#2159).
///
/// A record starts at a `[<topic>] ` line and runs to the next such line. Lines
/// before the first record form a record of their own. Text with no record
/// line is one record, so a change to ICM's format degrades to per-action
/// behavior and never loses text. Each record has trailing whitespace removed,
/// and empty records are dropped.
pub(crate) fn split_recall_records(text: &str) -> Vec<String> {
    let mut records: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        match records.last_mut() {
            Some(current) if !is_record_start(line) => current.push(line),
            _ => records.push(vec![line]),
        }
    }
    records
        .into_iter()
        .map(|lines| lines.join("\n").trim_end().to_string())
        .filter(|record| !record.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn arb_records() -> impl Strategy<Value = Vec<(String, String)>> {
        proptest::collection::vec(
            (
                prop_oneof![
                    Just(SCOPE_CONTEXT_TOPIC.to_string()),
                    Just("web-fetch".to_string()),
                    Just("web-fetch-proj".to_string()),
                    "[a-z]{1,8}",
                    "[a-zé日]{1,4}",
                ],
                "[a-z][a-z ]{0,12}[a-z]",
            ),
            0..6,
        )
    }

    proptest! {
        /// Only a scope-context record or a fetched page is unscoped.
        #[test]
        fn only_scope_context_and_page_records_are_unscoped(records in arb_records()) {
            for (topic, body) in &records {
                let record = super::super::Recalled { id: None, text: format!("[{topic}] {body}") };
                let expected = topic == SCOPE_CONTEXT_TOPIC
                    || topic == "web-fetch"
                    || topic.starts_with("web-fetch-");
                prop_assert_eq!(is_unscoped(&record), expected, "{}", topic);
            }
        }

        /// A record line carries a topic of 1 to 200 characters, counted as characters, not
        /// bytes, so a topic in a script with wide characters is not cut short.
        #[test]
        fn the_topic_bound_counts_characters(n in 0usize..=203, wide in any::<bool>()) {
            let c = if wide { "日" } else { "a" };
            let line = format!("[{}] body", c.repeat(n));
            prop_assert_eq!(is_record_start(&line), (1..=MAX_TOPIC_CHARS).contains(&n));
        }
    }
}
