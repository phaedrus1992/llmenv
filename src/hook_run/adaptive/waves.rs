//! The recall waves of one prompt: the project recall and the global topic recall, then the
//! keyword and topic fanouts.

use std::time::Instant;

use super::{AdaptiveCtx, FANOUT_LIMIT, MAIN_LIMIT, Outcome, WAVE2_CUTOFF, query, recall};
use crate::hook_run::action::{RecallQuery, Reply};
use crate::hook_run::recall::RecallBudget;
use crate::hook_run::relevance;

/// Recalls `text` into `budget`, in budget order: the session project's main recall, the
/// recall of the global topics, keyword fanout, topic fanout. The flag says whether the main
/// recalls ran and none of them failed. Only the global topics are read across projects, so
/// the records of another project stay out (#2950). With no session project, the fanouts do
/// not run.
///
/// The fanouts run only when the main recalls leave room in `budget`. Their records come
/// after the main records, so a full budget would drop them all (#2920). No call runs when
/// `budget` is full before the main recalls.
pub(super) async fn waves(
    ctx: &AdaptiveCtx<'_>,
    text: &str,
    keywords: &[String],
    project: Option<&str>,
    budget: &mut RecallBudget,
) -> (Outcome, bool) {
    let in_project = |q: RecallQuery| {
        project.map(|p| RecallQuery {
            project: Some(p.to_string()),
            ..q
        })
    };
    let client = ctx.client;
    // The scope fallback can fill the budget before this runs. Every record would then be
    // dropped, so no call runs. The query is not finished, so the next turn runs it again.
    if budget.is_full() {
        let has_global = ctx.global_topics.iter().any(|t| !t.trim().is_empty());
        let fanouts = project.map_or(0, |_| 1 + keywords.len().min(2));
        budget.skip_actions(usize::from(has_global) + fanouts);
        return (Outcome::default(), false);
    }
    let start = Instant::now();
    let (own, main) = tokio::join!(
        recall(client, in_project(query(text, MAIN_LIMIT))),
        crate::hook_run::global_topics::recall(client, text, ctx.global_topics, MAIN_LIMIT),
    );
    // A failed project recall is not a finished query, even when a global topic answered: the
    // skip rule would reuse it and the project's records would stay missing.
    let main_ok = (own.is_some() || main.is_some())
        && !matches!(own, Some(Err(_)))
        && !matches!(main, Some(Err(_)));
    let mut outcome = Outcome::default();
    push_into(&mut outcome, budget, [own, main]);
    let topics: Vec<String> = outcome
        .records
        .iter()
        .filter_map(|r| r.topic().map(str::to_string))
        .collect();
    let siblings = relevance::sibling_topics(&topics);
    let keyword = |i: usize| {
        keywords.get(i).and_then(|k| {
            in_project(RecallQuery {
                keyword: Some(k.clone()),
                ..query(text, FANOUT_LIMIT)
            })
        })
    };
    let topic = |i: usize| {
        siblings.get(i).and_then(|t| {
            in_project(RecallQuery {
                topic: Some(t.clone()),
                ..query(text, FANOUT_LIMIT)
            })
        })
    };
    let fanouts = [keyword(0), keyword(1), topic(0), topic(1)];
    if budget.is_full() {
        budget.skip_actions(fanouts.iter().flatten().count());
    } else if start.elapsed() < WAVE2_CUTOFF {
        let [k0, k1, t0, t1] = fanouts;
        let (k0, k1, t0, t1) = tokio::join!(
            recall(client, k0),
            recall(client, k1),
            recall(client, t0),
            recall(client, t1),
        );
        push_into(&mut outcome, budget, [k0, k1, t0, t1]);
    } else {
        // A slow server would push the prompt past the latency it tolerates. The skip is not a
        // budget skip, so it is logged rather than counted in the budget notice.
        tracing::error!(
            "adaptive recall: {} fanout recall(s) skipped, the main recalls took {:?} (cutoff {:?})",
            fanouts.iter().flatten().count(),
            start.elapsed(),
            WAVE2_CUTOFF
        );
    }
    (outcome, main_ok)
}

/// Records each result in `outcome`, and adds each new record to `budget`.
fn push_into(
    outcome: &mut Outcome,
    budget: &mut RecallBudget,
    results: impl IntoIterator<Item = Option<anyhow::Result<Reply>>>,
) {
    let before = outcome.records.len();
    results.into_iter().for_each(|r| outcome.push(r));
    budget.add_records(outcome.records.get(before..).unwrap_or_default().to_vec());
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::hook_run::mcp_client::McpHttpClient;
    use crate::hook_run::recall::RECALL_BUDGET_BYTES;

    async fn run(server: &MockServer, project: Option<&str>, topics: &[String]) -> (Outcome, bool) {
        let (outcome, ok, _) = run_with_keywords(server, project, topics, &[]).await;
        (outcome, ok)
    }

    async fn run_with_keywords(
        server: &MockServer,
        project: Option<&str>,
        topics: &[String],
        keywords: &[String],
    ) -> (Outcome, bool, RecallBudget) {
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::hook_run::session_ledger::LedgerStore::new(dir.path());
        let payload = json!({});
        let ctx = AdaptiveCtx {
            client: &client,
            store: &store,
            session_id: "s1",
            payload: &payload,
            global_topics: topics,
            state_dir: dir.path(),
        };
        let mut budget = RecallBudget::default();
        let (outcome, ok) = waves(&ctx, "q", keywords, project, &mut budget).await;
        (outcome, ok, budget)
    }

    /// Matches a request whose body names no topic.
    struct NoTopic;

    impl wiremock::Match for NoTopic {
        fn matches(&self, request: &wiremock::Request) -> bool {
            !String::from_utf8_lossy(&request.body).contains(r#""topic""#)
        }
    }

    fn topics(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    async fn mount(server: &MockServer, needle: &str, body: &str) {
        Mock::given(method("POST"))
            .and(body_string_contains(needle))
            .respond_with(crate::hook_run::test_recall::ToolReply(body.to_string()))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_page_fetched_in_another_project_is_not_recalled() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        mount(
            &server,
            r#""project":"proj-b""#,
            "[web-fetch-proj-b] URL: https://own.example",
        )
        .await;
        mount(
            &server,
            r#""project":"""#,
            "[web-fetch-proj-a] URL: https://other.example\n[web-fetch] URL: \
             https://old.example\n[context-proj-a] a fact",
        )
        .await;
        let (outcome, main_ok) = run(&server, Some("proj-b"), &topics(&["preferences"])).await;
        assert!(main_ok);
        let all = outcome.text();
        assert!(all.contains("https://own.example"), "{all}");
        assert!(all.contains("a fact"), "{all}");
        assert!(!all.contains("https://other.example"), "{all}");
        assert!(!all.contains("https://old.example"), "{all}");
    }

    #[tokio::test]
    async fn a_scope_record_of_another_project_is_not_recalled() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        mount(
            &server,
            r#""project":"""#,
            "[llmenv-scope-context] tags: lang-go, bundles: go-dev\n[context-proj-a] a fact",
        )
        .await;
        let (outcome, main_ok) = run(&server, None, &topics(&["preferences"])).await;
        assert!(main_ok);
        let all = outcome.text();
        assert!(all.contains("a fact"), "{all}");
        assert!(!all.contains("lang-go"), "{all}");
    }

    /// The main recall reads the session's project and the global topics only, so a record of
    /// another project in a topic that is not global stays out (#2950).
    #[tokio::test]
    async fn only_the_global_topics_are_read_across_projects() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        let reply = crate::hook_run::test_recall::ToolReply("[preferences] be terse".to_string());
        Mock::given(method("POST"))
            .and(body_string_contains(r#""topic":"preferences""#))
            .respond_with(reply)
            .with_priority(1)
            .mount(&server)
            .await;
        // An untopiced recall across every project is the leak, so only it answers with a record
        // of another project. A topic recall reads one topic, which the mock cannot filter.
        let reply = crate::hook_run::test_recall::ToolReply(
            "[context-proj-a] other project fact".to_string(),
        );
        Mock::given(method("POST"))
            .and(body_string_contains(r#""project":"""#))
            .and(NoTopic)
            .respond_with(reply)
            .mount(&server)
            .await;
        mount(
            &server,
            r#""project":"proj-b""#,
            "[context-proj-b] own fact",
        )
        .await;

        let (outcome, main_ok) = run(&server, Some("proj-b"), &topics(&["preferences", " "])).await;
        assert!(main_ok);
        let all = outcome.text();
        assert!(
            all.contains("own fact") && all.contains("be terse"),
            "{all}"
        );
        assert!(!all.contains("other project fact"), "{all}");

        let (outcome, _) = run(&server, Some("proj-b"), &[]).await;
        let all = outcome.text();
        assert!(
            all.contains("own fact") && !all.contains("other project fact"),
            "{all}"
        );
    }

    /// The keyword and sibling-topic fanouts read the session project only, so no request reads
    /// every project when no global topic is set (#2950).
    #[tokio::test]
    async fn the_fanouts_read_only_the_session_project() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        mount(
            &server,
            r#""project":"""#,
            "[context-proj-a] other project fact",
        )
        .await;
        mount(
            &server,
            r#""project":"proj-b""#,
            "[context-proj-b] own fact",
        )
        .await;

        let keywords = topics(&["k1", "k2"]);
        let (outcome, main_ok, _) =
            run_with_keywords(&server, Some("proj-b"), &[], &keywords).await;
        assert!(main_ok);
        let all = outcome.text();
        assert!(all.contains("own fact"), "{all}");
        assert!(!all.contains("other project fact"), "{all}");
        // The main recall, two keyword fanouts, and two sibling-topic fanouts.
        assert_eq!(outcome.calls, 5, "{outcome:?}");
    }

    /// With no session project, only the global topics are read.
    #[tokio::test]
    async fn no_session_project_runs_no_fanout() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        mount(
            &server,
            r#""topic":"preferences""#,
            "[preferences] be terse",
        )
        .await;

        let keywords = topics(&["k1"]);
        let (outcome, main_ok, _) =
            run_with_keywords(&server, None, &topics(&["preferences"]), &keywords).await;
        assert!(main_ok);
        assert_eq!(outcome.calls, 1, "{outcome:?}");
        assert!(outcome.text().contains("be terse"));
    }

    /// A failed project recall is not a finished query, even when a global topic answered.
    #[tokio::test]
    async fn a_failed_project_recall_is_not_ok_beside_a_global_topic() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        mount(
            &server,
            r#""topic":"preferences""#,
            "[preferences] be terse",
        )
        .await;
        Mock::given(method("POST"))
            .and(body_string_contains(r#""project":"proj-b""#))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (outcome, main_ok) = run(&server, Some("proj-b"), &topics(&["preferences"])).await;
        assert!(!main_ok, "{outcome:?}");
        assert!(outcome.text().contains("be terse"));
    }

    /// Fanout records only fill the bytes that the main recalls leave, so a full budget skips
    /// the fanouts and counts them as skipped (#2920).
    #[tokio::test]
    async fn a_full_budget_skips_the_fanouts() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        let big = format!("[context-proj-b] {}", "x".repeat(RECALL_BUDGET_BYTES));
        let fill = format!("[context-proj-b] {}", "y".repeat(RECALL_BUDGET_BYTES - 150));
        mount(&server, r#""project":"proj-b""#, &format!("{fill}\n{big}")).await;

        let keywords = topics(&["k1", "k2"]);
        let (outcome, main_ok, budget) =
            run_with_keywords(&server, Some("proj-b"), &[], &keywords).await;
        assert!(main_ok);
        assert_eq!(outcome.calls, 1, "calls={}", outcome.calls);
        let notice = budget.notice().unwrap_or_default();
        assert!(notice.contains("4 recall queries skipped"), "{notice}");
    }

    /// The scope fallback can fill the budget first; then no recall runs and the query is not
    /// marked finished (#2920).
    #[tokio::test]
    async fn a_budget_full_before_the_waves_runs_no_recall() {
        let server = MockServer::start().await;
        mount(&server, "initialize", "").await;
        mount(
            &server,
            r#""project":"proj-b""#,
            "[context-proj-b] own fact",
        )
        .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::hook_run::session_ledger::LedgerStore::new(dir.path());
        let payload = json!({});
        let global = topics(&["preferences"]);
        let ctx = AdaptiveCtx {
            client: &client,
            store: &store,
            session_id: "s1",
            payload: &payload,
            global_topics: &global,
            state_dir: dir.path(),
        };
        let mut budget = RecallBudget::default();
        budget.add_text(&format!("[scope] {}", "z".repeat(RECALL_BUDGET_BYTES - 50)));
        assert!(budget.is_full());
        let keywords = topics(&["k1"]);
        let (outcome, main_ok) = waves(&ctx, "q", &keywords, Some("proj-b"), &mut budget).await;
        assert!(!main_ok);
        assert_eq!(outcome.calls, 0);
        let notice = budget.notice().unwrap_or_default();
        assert!(notice.contains("3 recall queries skipped"), "{notice}");
        let asked = server.received_requests().await.unwrap_or_default();
        assert!(
            asked
                .iter()
                .all(|r| String::from_utf8_lossy(&r.body).contains("initialize")
                    || !String::from_utf8_lossy(&r.body).contains("icm_memory_recall")),
            "no recall request"
        );
    }
}
