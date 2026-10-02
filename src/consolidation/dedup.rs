//! Project scoping and duplicate detection for consolidation rules (#2387).
//!
//! ICM filters a recall by project through the topic name, so the topic that
//! holds the rules must carry the project name.

use std::collections::BTreeSet;

use crate::hook_run::mcp_client::McpHttpClient;

/// Word-overlap ratio at which two rules count as one rule. The model words the
/// same rule a little differently each session, so an exact match finds almost
/// no duplicates, and a ratio below this value starts to merge distinct rules.
const SIMILARITY_THRESHOLD: f64 = 0.8;

/// Number of stored rules to compare a new rule against.
const RECALL_LIMIT: u32 = 5;

/// The topic that held every project's rules before v3.12.0.
const LEGACY_TOPIC: &str = "llmenv-consolidation";

/// Words that reverse the meaning of a rule. Two rules that differ in these
/// words are opposite rules, however many other words they share.
const NEGATIONS: &[&str] = &[
    "not", "never", "no", "t", "without", "avoid", "cannot", "dont", "nothing",
];

/// The topic that holds the consolidation rules of `project`.
pub(super) fn rule_topic(project: &str) -> String {
    format!("llmenv-consolidation-{project}")
}

fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Jaccard ratio of the word sets of `a` and `b`; `0.0` when either has no words.
fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (words(a), words(b));
    let union = a.union(&b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / union as f64
}

fn negations(text: &str) -> BTreeSet<String> {
    let mut found = words(text);
    found.retain(|w| NEGATIONS.contains(&w.as_str()));
    found
}

/// Whether `rule` matches any of `existing` at or above the threshold, with
/// the same negation words.
pub(super) fn matches_any<'a>(rule: &str, existing: impl IntoIterator<Item = &'a str>) -> bool {
    let rule_negations = negations(rule);
    existing
        .into_iter()
        .any(|e| similarity(rule, e) >= SIMILARITY_THRESHOLD && negations(e) == rule_negations)
}

/// Whether ICM already holds a rule that matches `rule`, in the project's topic
/// or in the legacy topic of v3.11 and earlier.
///
/// ICM matches a topic filter as a substring, so each hit is checked against
/// the exact topic.
///
/// # Errors
/// Returns the MCP error when a recall call fails.
pub(super) async fn is_stored(
    client: &McpHttpClient,
    project: &str,
    rule: &str,
) -> anyhow::Result<bool> {
    // The legacy rules of every project share one topic, so that recall has no project filter.
    for (topic, scope) in [
        (rule_topic(project), project),
        (LEGACY_TOPIC.to_string(), ""),
    ] {
        let args = serde_json::json!({
            "query": rule,
            "topic": topic,
            "project": scope,
            "limit": RECALL_LIMIT,
        });
        let output = client.call_tool("icm_memory_recall", args).await?;
        let stored = super::parse_recall_output(&output);
        let in_topic = stored.iter().filter(|r| {
            r.topic
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(&topic))
        });
        if matches_any(rule, in_topic.map(|r| r.summary.as_str())) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use std::time::Duration;

    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn rule_topic_carries_the_project_name() {
        assert_eq!(rule_topic("llmenv"), "llmenv-consolidation-llmenv");
    }

    #[test]
    fn a_reworded_rule_matches_and_a_different_rule_does_not() {
        let stored = ["Always run cargo fmt before you commit the changes"];
        assert!(matches_any(
            "always run cargo fmt before you commit changes.",
            stored
        ));
        assert!(!matches_any(
            "Use tracing, never println, in library crates",
            stored
        ));
        assert!(!matches_any("", stored));
        assert!(!matches_any("anything", []));
    }

    #[test]
    fn opposite_rules_are_not_duplicates() {
        let stored = ["Always commit the lockfile with the change and run the full test suite"];
        assert!(!matches_any(
            "Never commit the lockfile with the change and run the full test suite",
            stored
        ));
        assert!(!matches_any(
            "Always commit the lockfile with the change and don't run the full test suite",
            stored
        ));
    }

    proptest::proptest! {
        #[test]
        fn similarity_is_symmetric_and_bounded(a in ".{0,60}", b in ".{0,60}") {
            let s = similarity(&a, &b);
            proptest::prop_assert!((0.0..=1.0).contains(&s));
            proptest::prop_assert!((s - similarity(&b, &a)).abs() < f64::EPSILON);
        }

        #[test]
        fn a_rule_with_words_matches_itself(a in "[a-z]{1,8}( [a-z]{1,8}){0,8}") {
            proptest::prop_assume!(negations(&a).is_empty());
            proptest::prop_assert!((similarity(&a, &a) - 1.0).abs() < f64::EPSILON);
            proptest::prop_assert!(matches_any(&a, [a.as_str()]));
        }

        #[test]
        fn nothing_matches_an_empty_store_or_an_empty_rule(a in ".{0,40}") {
            proptest::prop_assert!(!matches_any(&a, []));
            proptest::prop_assert!(!matches_any("", [a.as_str()]));
        }
    }

    fn recall_reply(body: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc": "2.0", "id": 1,
            "result": {"content": [{"type": "text", "text": body}]}})
    }

    async fn server(recall_body: &str) -> MockServer {
        let server = MockServer::start().await;
        for (needle, body) in [("initialize", ""), ("icm_memory_recall", recall_body)] {
            Mock::given(method("POST"))
                .and(body_string_contains(needle))
                .respond_with(ResponseTemplate::new(200).set_body_json(recall_reply(body)))
                .mount(&server)
                .await;
        }
        server
    }

    #[tokio::test]
    async fn is_stored_compares_against_the_recalled_rules() {
        let hit = "--- r1 [score: 0.9] ---\n  topic: llmenv-consolidation-p\n  importance: high\n  weight: 1.0\n  \
                   summary: Always run cargo fmt before commit\n";
        for (body, expected) in [(hit, true), ("", false)] {
            let server = server(body).await;
            let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
            let got = is_stored(&client, "p", "Always run cargo fmt before commit")
                .await
                .unwrap();
            assert_eq!(got, expected, "{body:?}");
        }
    }

    #[tokio::test]
    async fn a_rule_in_another_topic_is_not_a_duplicate() {
        // ICM matches a topic filter as a substring, so a longer project name can leak in.
        let other = "--- r1 ---\n  topic: llmenv-consolidation-p-core\n  importance: high\n  \
                     weight: 1.0\n  summary: Always run cargo fmt before commit\n";
        let server = server(other).await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let got = is_stored(&client, "p", "Always run cargo fmt before commit")
            .await
            .unwrap();
        assert!(!got);
    }

    #[tokio::test]
    async fn a_legacy_topic_rule_is_a_duplicate() {
        let legacy = "--- r1 ---\n  topic: llmenv-consolidation\n  importance: high\n  \
                      weight: 1.0\n  summary: Always run cargo fmt before commit\n";
        let server = server(legacy).await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let got = is_stored(&client, "p", "Always run cargo fmt before commit")
            .await
            .unwrap();
        assert!(got);
    }
}
