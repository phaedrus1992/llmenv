use super::*;

#[tokio::test]
async fn session_start_adds_project_memories_and_drops_cut_records() {
    let server = server_with(&[
        (
            "icm_wake_up",
            "## Identity\n[preferences] global fact\n[preferences] cut mid […]",
        ),
        (
            "\"project\":\"proj-a\"",
            "[decisions-a] project fact\n[llmenv-scope-context] scope blob\n[d] cut […]",
        ),
    ])
    .await;
    let f = fixture(&server);
    let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
        max_tokens: None,
        project: Some("proj-a".into()),
    });
    let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
        .await
        .unwrap()
        .text();
    assert!(out.contains("project fact"), "{out}");
    assert!(out.contains("global fact"), "{out}");
    assert!(
        !out.contains("scope blob"),
        "scope records come from the scope set: {out}"
    );
    assert!(!out.contains("cut"), "a cut record is dropped whole: {out}");
}

#[tokio::test]
async fn project_memories_cut_by_the_budget_are_not_a_failure_and_the_notice_names_the_budget() {
    let server = server_with(&[]).await;
    let mut f = fixture(&server);
    f.client = f.client.clone().with_recall_deadline(Some(
        crate::hook_run::recall_deadline::Deadline::expiring_in(Duration::ZERO),
    ));
    let (text, answered) = project_memories(&f.client, "proj-a").await;
    assert_eq!((text.as_str(), answered), ("", None));
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("session start recall budget"), "{notices}");
}

#[tokio::test]
async fn wake_up_drops_only_the_cut_bullet() {
    // icm writes wake-up items as bullets under headings, not as `[topic]` records.
    let server = server_with(&[(
            "icm_wake_up",
            "# ICM Wake-up (project: proj-a)\n\n## Identity\n- kept preference\n- cut item in the fyi re […]\n",
        )])
        .await;
    let f = fixture(&server);
    let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
        max_tokens: None,
        project: Some("proj-a".into()),
    });
    let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
        .await
        .unwrap()
        .text();
    assert!(out.contains("## Identity"), "{out}");
    assert!(out.contains("kept preference"), "{out}");
    assert!(
        !out.contains("cut item"),
        "a cut bullet is dropped alone: {out}"
    );
    let notice = agent_notice::current().unwrap_or_default();
    assert!(
        notice.contains("1 memory item(s) were dropped"),
        "the drop must reach the agent: {notice}"
    );
}

#[test]
fn the_cut_notice_names_the_count_and_appears_only_when_items_were_cut() {
    assert_eq!(cut_items_notice(0), None);
    assert!(cut_items_notice(1).is_some_and(|n| n.contains("1 memory item(s)")));
    assert!(cut_items_notice(2).is_some_and(|n| n.contains("2 memory item(s)")));
}

#[test]
fn the_pack_budget_counts_the_project_memories_when_wake_up_fails() {
    let failed: anyhow::Result<String> = Err(anyhow::anyhow!("icm down"));
    assert_eq!(pack_bytes(&failed, "abc"), 5);
    let loaded: anyhow::Result<String> = Ok("hello".to_string());
    assert_eq!(pack_bytes(&loaded, ""), 7);
}

#[tokio::test]
async fn a_resumed_session_skips_the_project_recall() {
    // A resumed session already holds its pack, so the project recall must not run.
    let server = server_with(&[("\"project\":\"proj-a\"", "[decisions-a] project fact")]).await;
    let f = fixture(&server);
    let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
        max_tokens: None,
        project: Some("proj-a".into()),
    });
    let out = session_start(&ctx(&f, &json!({"source": "resume"})), wake, vec![])
        .await
        .unwrap()
        .text();
    assert!(
        !out.contains("project fact"),
        "a resume sends no pack: {out}"
    );
}

#[tokio::test]
async fn a_failed_project_recall_keeps_the_wake_up_pack() {
    let server = server_with(&[("icm_wake_up", "## Identity\n- global fact")]).await;
    let f = fixture(&server);
    let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
        max_tokens: None,
        project: Some("proj-b".into()),
    });
    let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
        .await
        .unwrap()
        .text();
    assert!(
        out.contains("global fact"),
        "the wake-up pack still goes out: {out}"
    );
    let notice = agent_notice::current().unwrap_or_default();
    assert!(
        notice.contains("project memories were skipped"),
        "the skipped project recall must reach the agent: {notice}"
    );
}

#[tokio::test]
async fn a_failed_wake_up_keeps_the_project_memories() {
    // The wake-up mock is registered first, so the wake-up request gets its 500 response.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("icm_wake_up"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("\"project\":\"proj-a\""))
        .respond_with(crate::hook_run::test_recall::ToolReply(
            ("[decisions-a] project fact").to_string(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(crate::hook_run::test_recall::ToolReply(("").to_string()))
        .mount(&server)
        .await;
    let f = fixture(&server);
    let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
        max_tokens: None,
        project: Some("proj-a".into()),
    });
    let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
        .await
        .unwrap()
        .text();
    assert!(
        out.contains("project fact"),
        "the project memories must survive: {out}"
    );
    let notice = agent_notice::current().unwrap_or_default();
    assert!(
        notice.contains("wake-up skipped"),
        "the failed wake-up must reach the agent: {notice}"
    );
}

#[tokio::test]
async fn project_memories_skipped_by_an_open_breaker_gives_no_answer() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let mut f = fixture(&server);
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recall-breaker.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    std::fs::write(
        &path,
        format!(
            r#"{{"consecutive_failures":0,"open_until_unix":{}}}"#,
            now + 100
        ),
    )?;
    f.client = f.client.clone().with_recall_breaker(Some(path));
    let (text, answered) = project_memories(&f.client, "proj").await;
    assert!(text.is_empty());
    assert_eq!(answered, None);
    Ok(())
}
