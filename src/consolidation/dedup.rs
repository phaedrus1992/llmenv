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

/// Whether `rule` matches any of `existing` at or above the threshold.
pub(super) fn matches_any<'a>(rule: &str, existing: impl IntoIterator<Item = &'a str>) -> bool {
    existing
        .into_iter()
        .any(|e| similarity(rule, e) >= SIMILARITY_THRESHOLD)
}

/// Whether ICM already holds a rule that matches `rule` in the project's topic.
///
/// # Errors
/// Returns the MCP error when the recall call fails.
pub(super) async fn is_stored(
    client: &McpHttpClient,
    project: &str,
    rule: &str,
) -> anyhow::Result<bool> {
    let args = serde_json::json!({
        "query": rule,
        "topic": rule_topic(project),
        "project": project,
        "limit": RECALL_LIMIT,
    });
    let output = client.call_tool("icm_memory_recall", args).await?;
    let stored = super::parse_recall_output(&output);
    Ok(matches_any(rule, stored.iter().map(|r| r.summary.as_str())))
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
        let hit = "--- r1 [score: 0.9] ---\n  topic: t\n  importance: high\n  weight: 1.0\n  \
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
}
