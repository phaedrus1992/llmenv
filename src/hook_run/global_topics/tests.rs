use super::*;

fn topics(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

#[test]
fn each_named_topic_is_one_recall_across_projects() {
    let queries = topic_queries("q", &topics(&["preferences", " ", "errors-resolved "]), 4);
    let named: Vec<_> = queries
        .iter()
        .map(|q| q.topic.as_deref().unwrap())
        .collect();
    assert_eq!(named, ["preferences", "errors-resolved"]);
    assert!(queries.iter().all(|q| q.project.as_deref() == Some("")));
    assert!(queries.iter().all(|q| q.limit == 4 && q.query == "q"));
}

#[test]
fn the_stateless_turn_recall_never_reads_every_project() {
    let global = topics(&["preferences"]);
    let unscoped = turn_actions(vec![Action::Recall(String::new())], "q", &global);
    assert!(
        unscoped.iter().all(|a| !matches!(a, Action::Recall(_))),
        "the empty project recall reads every project: {unscoped:?}"
    );
    assert_eq!(unscoped.len(), 1, "{unscoped:?}");

    let scoped = turn_actions(vec![Action::Recall("proj".into())], "q", &global);
    assert!(matches!(&scoped[0], Action::Recall(p) if p == "proj"));
    assert_eq!(
        scoped.len(),
        2,
        "the project and the global topic: {scoped:?}"
    );

    assert!(
        turn_actions(vec![Action::Store], "q", &global).len() == 1,
        "no recall, no topics"
    );
    assert!(turn_actions(vec![Action::Recall(String::new())], "q", &[]).is_empty());
}

/// One failed topic beside one that answered keeps the answer and tells the agent which topic is
/// missing. Every topic failed is an error that names each topic.
#[tokio::test]
async fn a_failed_topic_beside_an_answer_is_noticed() {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let reply = |body: &str| crate::hook_run::test_recall::ToolReply(body.to_string());
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(reply(""))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(r#""topic":"preferences""#))
        .respond_with(reply("[preferences] be terse"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(r#""topic":"errors-resolved""#))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = McpHttpClient::test_new(server.uri(), std::time::Duration::from_secs(2)).unwrap();

    let found = recall(
        &client,
        "q",
        &topics(&["preferences", "errors-resolved"]),
        4,
    )
    .await
    .unwrap()
    .unwrap()
    .into_text();
    assert!(found.contains("be terse"), "{found}");
    let notice = crate::agent_notice::current().unwrap_or_default();
    assert!(notice.contains("`errors-resolved`"), "{notice}");

    let err = recall(&client, "q", &topics(&["errors-resolved"]), 4)
        .await
        .unwrap()
        .unwrap_err();
    assert!(format!("{err:#}").contains("errors-resolved"), "{err:#}");
}

proptest::proptest! {
    /// One recall per topic that is not blank after a trim, each across every project, and the
    /// stateless turn never keeps an empty-project recall.
    #[test]
    fn each_non_blank_topic_is_one_recall(
        names in proptest::collection::vec("[ \t]{0,2}[a-z-]{0,6}[ \t]{0,2}", 0..6),
        project in proptest::option::of("[a-z]{1,6}"),
    ) {
        let expected: Vec<&str> = names.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
        let queries = topic_queries("q", &names, 3);
        let got: Vec<&str> = queries.iter().map(|q| q.topic.as_deref().unwrap()).collect();
        proptest::prop_assert_eq!(got, expected);
        proptest::prop_assert!(queries.iter().all(|q| q.project.as_deref() == Some("")));
        let start = vec![Action::Recall(project.clone().unwrap_or_default())];
        let actions = turn_actions(start, "q", &names);
        proptest::prop_assert!(
            actions.iter().all(|a| !matches!(a, Action::Recall(p) if p.is_empty()))
        );
    }
}
