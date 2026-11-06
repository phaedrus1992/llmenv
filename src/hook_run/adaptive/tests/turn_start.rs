use super::*;

#[tokio::test]
async fn turn_start_falls_back_to_the_scope_set_without_session_start() {
    let server = server_with(&[("llmenv-tag:proj", "[context-p] scope fact")]).await;
    let f = fixture(&server);
    let turn = json!({"prompt": "hi"});
    let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")])
        .await
        .unwrap()
        .text();
    assert!(out.contains("scope fact"), "{out}");
    assert!(f.store.load("s1").unwrap().scope_sent(MAIN_AGENT));
}

#[tokio::test]
async fn a_repeated_query_with_no_new_activity_makes_no_relevance_calls() {
    let server = server_with(&[("\"limit\":10", "[context-p] fact")]).await;
    let f = fixture(&server);
    f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
    let turn = json!({"prompt": "same"});
    turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
    let calls_before = server.received_requests().await.unwrap().len();
    turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        calls_before
    );
}

#[tokio::test]
async fn topic_fanout_recalls_sibling_topics() {
    let server = server_with(&[
        ("\"limit\":10", "[context-llmenv] main fact"),
        ("decisions-llmenv", "[decisions-llmenv] sibling fact"),
    ])
    .await;
    let f = fixture(&server);
    f.store.update("s1", |l| {
        l.set_scope_sent(MAIN_AGENT);
        l.project = Some("llmenv".to_string());
    });
    let turn = json!({"prompt": "why"});
    let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
    assert!(
        out.contains("main fact") && out.contains("sibling fact"),
        "{out}"
    );
}

#[tokio::test]
async fn one_failed_call_keeps_the_others() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("decisions-p"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    for (needle, body) in [
        ("initialize", ""),
        ("\"limit\":10", "[context-p] kept fact"),
    ] {
        Mock::given(method("POST"))
            .and(body_string_contains(needle))
            .respond_with(crate::hook_run::test_recall::ToolReply((body).to_string()))
            .mount(&server)
            .await;
    }
    let f = fixture(&server);
    f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
    let turn = json!({"prompt": "x"});
    let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
    assert!(out.contains("kept fact"), "{out}");
}

#[tokio::test]
async fn every_adaptive_recall_names_a_project_filter() {
    // The default project filter is the ICM server's own cwd, which says
    // nothing about this session when ICM runs remotely (AGENTS.md). A session with no
    // project reads the global topics only, across every project (#2950).
    let server = server_with(&[("\"limit\":10", "[context-p] fact")]).await;
    let f = fixture(&server);
    f.store.update("s1", |l| {
        l.set_scope_sent(MAIN_AGENT);
        l.push_activity(relevance::activity_from_tool_call(
            "Bash",
            &json!({"command": "cargo test"}),
            1,
        ));
    });
    turn_start(&ctx(&f, &json!({"prompt": "why"})), vec![])
        .await
        .unwrap()
        .text();
    tool_failure(&ctx(&f, &json!({"tool_name": "Bash", "error": "boom"})))
        .await
        .unwrap()
        .text();
    let recalls: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .filter(|b| b.contains("icm_memory_recall"))
        .collect();
    assert!(recalls.len() >= 3, "{recalls:?}");
    for body in &recalls {
        assert!(
            body.contains("\"project\":\"\"") && body.contains("\"topic\""),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_failed_fallback_is_not_retried_on_every_turn() {
    let server = failing_server().await;
    let f = fixture(&server);
    turn_start(&ctx(&f, &json!({"prompt": "hi"})), vec![tag_action("proj")])
        .await
        .unwrap()
        .text();
    let ledger = f.store.load("s1").unwrap();
    assert!(
        ledger.scope_sent(MAIN_AGENT),
        "a failed fallback is tried once, not on every turn (#2897)"
    );
    assert_eq!(
        ledger.last_query_hash, None,
        "a failed recall is not a done query"
    );
}

#[test]
fn a_failure_with_nothing_unrun_has_no_notice() {
    assert_eq!(unrun_notice(0, true, false), None);
    assert_eq!(unrun_notice(0, false, true), None);
}

#[test]
fn a_failure_that_leaves_tags_unrun_names_the_count() {
    let notice = unrun_notice(3, true, false).unwrap();
    assert!(
        notice.contains("3 scope recall action(s) did not run"),
        "{notice}"
    );
    assert_eq!(unrun_notice(3, false, false), None);
}

#[tokio::test]
async fn a_failed_scope_call_ends_the_fallback_for_this_turn() {
    let server = failing_server().await;
    let f = fixture(&server);
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, std::collections::BTreeSet::new());
    let (outcome, unrun) = run_scope(
        &ctx(&f, &json!({"prompt": "hi"})),
        vec![tag_action("a"), tag_action("b"), tag_action("c")],
        &mut budget,
    )
    .await;
    assert_eq!(outcome.calls, 1, "{outcome:?}");
    let notice = unrun.expect("the stop after a failure must be reported");
    assert!(notice.contains("2 scope recall action(s)"), "{notice}");
}

#[tokio::test]
async fn a_typo_failure_sends_no_recall_so_no_other_project_is_injected() {
    let server = MockServer::start().await;
    let f = fixture(&server);
    let payload = json!({
        "tool_name": "Bash",
        "error": "Exit code 127\nzsh: command not found: gti"
    });
    let text = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
    assert!(text.is_empty(), "{text}");
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "{requests:?}");
}

#[tokio::test]
async fn waves_count_every_failed_call() {
    let server = failing_server().await;
    let f = fixture(&server);
    let payload = serde_json::json!({});
    let mut budget = crate::hook_run::recall::RecallBudget::default();
    let (outcome, main_ok) = waves(
        &ctx(&f, &payload),
        "q",
        &["k".to_string()],
        Some("proj"),
        &mut budget,
    )
    .await;
    assert!(!main_ok);
    assert!(outcome.all_failed(), "{outcome:?}");
    // The project recall, the global topics, and one keyword fanout.
    assert_eq!(outcome.calls, 3);
}

#[test]
fn outage_notice_only_when_every_call_failed() {
    let mut outcome = Outcome::default();
    assert_eq!(outage_notice("turn_start", &outcome), None);
    outcome.calls = 3;
    outcome.failed = 2;
    assert_eq!(outage_notice("turn_start", &outcome), None);
    outcome.failed = 3;
    assert_eq!(
        outage_notice("turn_start", &outcome).as_deref(),
        Some(
            "llmenv: memory turn_start recall skipped: all 3 ICM calls failed. Fix: run \
                 `llmenv doctor` to check the memory server."
        )
    );
}

#[tokio::test]
async fn turn_start_sends_activity_terms_as_keyword_filters() {
    let server = server_with(&[("\"keyword\":\"recall\"", "[k] keyword fact")]).await;
    let f = fixture(&server);
    f.store.update("s1", |l| {
        l.set_scope_sent(MAIN_AGENT);
        l.project = Some("proj".to_string());
        l.push_activity(crate::hook_run::session_ledger::Activity {
            tool: "Read".to_string(),
            target: Some("/r/src/hook_run/recall.rs".to_string()),
            at: 1,
        });
    });
    let payload = json!({"prompt": "work on it"});
    let out = turn_start(&ctx(&f, &payload), Vec::new())
        .await
        .unwrap()
        .text();
    assert!(out.contains("keyword fact"), "{out}");
}

/// Each hook adds its recall calls to the health ring that doctor reads (#2919).
#[tokio::test]
async fn a_hook_records_its_recall_calls_for_doctor() {
    let server = failing_server().await;
    let f = fixture(&server);
    f.store
        .update("s1", |l| l.project = Some("proj".to_string()));
    let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
    tool_failure(&ctx(&f, &payload)).await.unwrap();
    let health = crate::hook_run::recall_health::health(f._dir.path())
        .unwrap()
        .expect("the hook wrote the ring");
    assert!(health.calls > 0, "{health:?}");
    assert_eq!(health.failed, health.calls, "{health:?}");
}

/// A reply slow enough that the main recalls pass the wave 2 cutoff.
struct SlowReply(crate::hook_run::test_recall::ToolReply);

impl wiremock::Respond for SlowReply {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        self.0
            .respond(request)
            .set_delay(WAVE2_CUTOFF + Duration::from_millis(100))
    }
}

/// The notice list is shared by the tests of one process, so the two tests on the skip notice
/// take turns.
static SKIP_NOTICE_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn slow_main_recalls_skip_the_fanouts_and_tell_the_agent() {
    let _turn = SKIP_NOTICE_TURN.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(crate::hook_run::test_recall::ToolReply(String::new()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(SlowReply(crate::hook_run::test_recall::ToolReply(
            "[context-llmenv] slow fact".to_string(),
        )))
        .mount(&server)
        .await;
    let f = fixture(&server);
    f.store.update("s1", |l| {
        l.set_scope_sent(MAIN_AGENT);
        l.project = Some("llmenv".to_string());
    });
    let turn = json!({"prompt": "slow"});
    turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("extra memory recall(s) were skipped"),
        "{notices}"
    );
}

#[tokio::test]
async fn slow_main_recalls_with_no_fanout_give_no_skip_notice() {
    let _turn = SKIP_NOTICE_TURN.lock().await;
    crate::agent_notice::forget_containing("extra memory recall");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(crate::hook_run::test_recall::ToolReply(String::new()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(SlowReply(crate::hook_run::test_recall::ToolReply(
            String::new(),
        )))
        .mount(&server)
        .await;
    let f = fixture(&server);
    // No project means no fanout, so the slow recalls skip nothing.
    f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
    let turn = json!({"prompt": "slow"});
    turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(!notices.contains("extra memory recall"), "{notices}");
}
