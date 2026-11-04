use super::*;

#[tokio::test]
async fn ensure_transcript_session_notices_a_map_it_cannot_read() {
    // #2658: a map that cannot be read must not start a transcript that cannot be linked.
    // The agent is told, because stderr from a hook never reaches it (#2818).
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    std::fs::write(&state_path, "{not json").unwrap();
    let cfg = SessionLog {
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            retention_days: None,
        }),
        ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
    };

    let id =
        ensure_transcript_session(&cfg, None, "claude-6", &ctx(), Some(&state_path), false).await;

    assert!(id.is_none());
    let notice = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notice.contains("no ICM transcript started for this session"),
        "{notice}"
    );
    assert!(notice.contains("Fix: run `llmenv doctor`"), "{notice}");
}

#[tokio::test]
async fn ensure_transcript_session_with_verify_false_skips_the_network_call() {
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    state::record_session_at(&state_path, "claude-5", "icm-sess-5").unwrap();
    // No mock mounted at all: any HTTP call here would fail the request.
    let server = MockServer::start().await;
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
    let cfg = SessionLog {
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            retention_days: None,
        }),
        ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
    };

    let id = ensure_transcript_session(
        &cfg,
        Some(&client),
        "claude-5",
        &ctx(),
        Some(&state_path),
        false,
    )
    .await;

    assert_eq!(id.as_deref(), Some("icm-sess-5"));
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "verify: false must not make any ICM call"
    );
}

#[tokio::test]
async fn ensure_transcript_session_reestablishes_when_cached_id_fails_verification() {
    // #1090: a cached icm_session_id must be revalidated against ICM, not
    // trusted forever — a stale one is cleared and replaced.
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    state::record_session_at(&state_path, "claude-4", "icm-stale").unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(
            crate::session_log::transcript::SHOW_TOOL,
        ))
        // The text icm-mcp sends for an unknown session: `session <id> not found`.
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc":"2.0","id":1,
            "result":{"isError":true,"content":[
                {"type":"text","text":"session icm-stale not found"}
            ]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(
            crate::session_log::transcript::START_TOOL,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-fresh")))
        .mount(&server)
        .await;
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
    let cfg = SessionLog {
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            retention_days: None,
        }),
        ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
    };

    let id = ensure_transcript_session(
        &cfg,
        Some(&client),
        "claude-4",
        &ctx(),
        Some(&state_path),
        true,
    )
    .await;

    assert_eq!(
        id.as_deref(),
        Some("icm-fresh"),
        "a cached id that fails verification must be replaced"
    );
    assert_eq!(
        state::lookup_session_at(&state_path, "claude-4")
            .unwrap()
            .as_deref(),
        Some("icm-fresh"),
        "the correlation map must be updated to the fresh id"
    );
}

#[tokio::test]
async fn ensure_transcript_session_keeps_link_when_icm_is_unreachable() {
    // #2667: a transient ICM outage must not orphan a valid transcript.
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    state::record_session_at(&state_path, "claude-5", "icm-live").unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(
            crate::session_log::transcript::SHOW_TOOL,
        ))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(
            crate::session_log::transcript::START_TOOL,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-new")))
        .expect(0)
        .mount(&server)
        .await;
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
    let cfg = SessionLog {
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            retention_days: None,
        }),
        ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
    };

    let id = ensure_transcript_session(
        &cfg,
        Some(&client),
        "claude-5",
        &ctx(),
        Some(&state_path),
        true,
    )
    .await;

    assert_eq!(id.as_deref(), Some("icm-live"));
    assert_eq!(
        state::lookup_session_at(&state_path, "claude-5")
            .unwrap()
            .as_deref(),
        Some("icm-live"),
        "the link must survive an unreachable ICM"
    );
}

#[tokio::test]
async fn ensure_transcript_session_starts_nothing_when_the_map_is_unwritable() {
    // #2667: a transcript that cannot be linked is an orphan, so none is started.
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(
            crate::session_log::transcript::START_TOOL,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-new")))
        .expect(0)
        .mount(&server)
        .await;
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
    let cfg = SessionLog {
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            retention_days: None,
        }),
        ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
    };
    // A directory where the lock file belongs makes the map unwritable. The lock step
    // resets the state dir mode, so a read-only dir would not stop the write.
    std::fs::create_dir(state_dir.path().join("transcript-sessions.lock")).unwrap();

    let id = ensure_transcript_session(
        &cfg,
        Some(&client),
        "claude-6",
        &ctx(),
        Some(&state_path),
        true,
    )
    .await;

    assert_eq!(id, None);
}

#[tokio::test]
async fn handle_session_log_session_end_reuses_correlated_session_id() {
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    state::record_session_at(&state_path, "claude-3", "icm-sess-3").unwrap();
    let log_dir = tempfile::tempdir().unwrap();
    let path = log_dir.path().join("session-log.jsonl");
    // transcript: false here only to avoid the detached-spawn side effect
    // in emit_session_log; the lookup itself (asserted via the return
    // value) doesn't depend on cfg.transcript.
    let cfg = file_only_cfg(&path);

    let id = handle_session_log(
        HookEvent::SessionEnd,
        &cfg,
        None,
        Some("claude-3"),
        &ctx(),
        Some(&state_path),
    )
    .await;

    assert_eq!(id.as_deref(), Some("icm-sess-3"));
    let lines = jsonl_lines(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["kind"], "lifecycle_end");
}
