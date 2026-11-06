use super::*;

#[tokio::test]
async fn session_start_sends_scope_once_and_turn_start_does_not_repeat_it() {
    let server = server_with(&[
        ("icm_wake_up", "wake pack"),
        ("llmenv-tag:proj", "[context-p] scope fact"),
        (
            "\"limit\":10",
            "[context-p] scope fact\n[context-p] relevant fact",
        ),
    ])
    .await;
    let f = fixture(&server);
    let start = json!({"source": "startup"});
    let out = session_start(
        &ctx(&f, &start),
        Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
        vec![tag_action("proj")],
    )
    .await
    .unwrap()
    .text();
    assert!(
        out.contains("wake pack") && out.contains("scope fact"),
        "{out}"
    );
    let turn = json!({"prompt": "work on recall"});
    let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")])
        .await
        .unwrap()
        .text();
    assert!(out.contains("relevant fact"), "{out}");
    assert!(!out.contains("scope fact"), "already sent: {out}");
}

#[tokio::test]
async fn compact_resets_and_resume_keeps_the_ledger() {
    let server = server_with(&[
        ("icm_wake_up", ""),
        ("llmenv-tag:proj", "[context-p] scope fact"),
    ])
    .await;
    let f = fixture(&server);
    let mut outputs = Vec::new();
    for source in ["startup", "resume", "compact"] {
        let payload = json!({ "source": source });
        let out = session_start(
            &ctx(&f, &payload),
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
            vec![tag_action("proj")],
        )
        .await
        .unwrap()
        .text();
        outputs.push(out.contains("scope fact"));
    }
    assert_eq!(
        outputs,
        [true, false, true],
        "startup sends, resume keeps, compact resets"
    );
}

// #2142: a resumed or forked session already holds the earlier wake-up pack.
#[tokio::test]
async fn resume_and_fork_skip_the_wake_up_call() {
    for source in ["resume", "fork"] {
        let server = server_with(&[("llmenv-tag:proj", "[context-p] scope fact")]).await;
        Mock::given(method("POST"))
            .and(body_string_contains("icm_wake_up"))
            .respond_with(crate::hook_run::test_recall::ToolReply(
                ("wake pack").to_string(),
            ))
            .expect(0)
            .mount(&server)
            .await;
        let f = fixture(&server);
        let payload = json!({ "source": source });
        let out = session_start(
            &ctx(&f, &payload),
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
            vec![tag_action("proj")],
        )
        .await
        .unwrap()
        .text();
        assert!(!out.contains("wake pack"), "{source}: {out}");
        server.verify().await;
    }
}

#[tokio::test]
async fn session_start_keeps_the_wake_pack_when_a_scope_call_fails() {
    let server = MockServer::start().await;
    for (needle, body) in [("initialize", ""), ("icm_wake_up", "wake pack")] {
        Mock::given(method("POST"))
            .and(body_string_contains(needle))
            .respond_with(crate::hook_run::test_recall::ToolReply((body).to_string()))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let f = fixture(&server);
    let start = json!({"source": "startup"});
    let out = session_start(
        &ctx(&f, &start),
        Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
        vec![tag_action("proj")],
    )
    .await
    .unwrap()
    .text();
    assert!(out.contains("wake pack"), "{out}");
    assert!(!f.store.load("s1").unwrap().scope_sent(MAIN_AGENT));
}

#[tokio::test]
async fn session_start_scope_budget_leaves_room_for_the_wake_pack() {
    let wake = "w".repeat(7_000);
    let scope: String = (0..10)
        .map(|i| format!("[context-p] scope fact {i} {}", "x".repeat(300)))
        .collect::<Vec<_>>()
        .join("\n");
    let server = server_with(&[("icm_wake_up", &wake), ("llmenv-tag:proj", &scope)]).await;
    let f = fixture(&server);
    let start = json!({"source": "startup"});
    let out = session_start(
        &ctx(&f, &start),
        Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
        vec![tag_action("proj")],
    )
    .await
    .unwrap()
    .text();
    assert!(out.len() <= RECALL_BUDGET_BYTES + 200, "{}", out.len());
    let sent = f.store.load("s1").unwrap().sent_for(MAIN_AGENT).len();
    assert!(sent < 10, "only records that fit are marked sent: {sent}");
}

#[tokio::test]
async fn the_wake_pack_takes_its_own_length_from_the_scope_budget() {
    let wake = "w".repeat(3_000);
    let scope = format!("[context-p] big scope fact {}", "x".repeat(3_000));
    let server = server_with(&[("icm_wake_up", &wake), ("llmenv-tag:proj", &scope)]).await;
    let f = fixture(&server);
    let out = session_start(
        &ctx(&f, &json!({"source": "startup"})),
        Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
        vec![tag_action("proj")],
    )
    .await
    .unwrap()
    .text();
    assert!(
        out.contains("big scope fact"),
        "the record fits: {}",
        out.len()
    );
}

async fn start_with_a_failed_wake(
    scope_body: Option<&str>,
    scope: Vec<Action>,
) -> anyhow::Result<crate::hook_run::HookText> {
    let pairs: Vec<(&str, &str)> = scope_body
        .map(|b| ("llmenv-tag:proj", b))
        .into_iter()
        .collect();
    let server = server_with(&pairs).await;
    let f = fixture(&server);
    session_start(
        &ctx(&f, &json!({"source": "startup"})),
        Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
        scope,
    )
    .await
}

#[tokio::test]
async fn a_failed_wake_with_a_working_scope_still_sends_the_scope() {
    let out = start_with_a_failed_wake(Some("[context-p] scope fact"), vec![tag_action("proj")])
        .await
        .unwrap()
        .text();
    assert!(out.contains("scope fact"), "{out}");
}

#[tokio::test]
async fn a_failed_wake_is_an_error_when_the_scope_also_fails() {
    assert!(
        start_with_a_failed_wake(None, vec![tag_action("proj")])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_failed_wake_is_an_error_when_there_is_no_scope() {
    assert!(start_with_a_failed_wake(None, Vec::new()).await.is_err());
}

#[tokio::test]
async fn the_session_project_ranks_first_after_session_start() {
    let server = server_with(&[
        ("\"project\":\"llmenv\"", "[decisions-llmenv] project fact"),
        ("\"limit\":10", "[preferences] global fact"),
    ])
    .await;
    let f = fixture(&server);
    let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
        max_tokens: None,
        project: Some("llmenv".into()),
    });
    session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
        .await
        .unwrap()
        .text();
    assert_eq!(
        f.store.load("s1").unwrap().project.as_deref(),
        Some("llmenv")
    );
    let out = turn_start(&ctx(&f, &json!({"prompt": "why"})), vec![])
        .await
        .unwrap()
        .text();
    let project_at = out.find("project fact").expect("project-scoped record");
    let global_at = out.find("global fact").expect("all-projects record");
    assert!(project_at < global_at, "{out}");
}

/// The wake-up and project-memories calls count in the health ring, so a dead server shows in
/// doctor even when the session has no scope action (#2919).
#[tokio::test]
async fn session_start_records_the_wake_up_and_project_calls() {
    let server = failing_server().await;
    let f = fixture(&server);
    let start = json!({"source": "startup"});
    let wake = crate::hook_run::action::WakeUpArgs {
        project: Some("proj".to_string()),
        ..Default::default()
    };
    let out = session_start(&ctx(&f, &start), Action::WakeUp(wake), Vec::new()).await;
    assert!(out.is_err(), "a failed wake-up is the hook's error");
    let health = crate::hook_run::recall_health::health(f._dir.path())
        .unwrap()
        .expect("session start wrote the ring");
    assert_eq!((health.calls, health.failed), (2, 2), "{health:?}");
}
