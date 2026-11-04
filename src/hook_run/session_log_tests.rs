use super::*;
use std::time::Duration;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ctx() -> ScopeContext {
    ScopeContext {
        tags: vec!["rust".into()],
        bundles: vec![],
        project: Some("llmenv".into()),
        cwd: "/tmp".into(),
        adapter: "claude-code".into(),
        llmenv_version: "3.0.0".into(),
        claude_code_version: String::new(),
    }
}

fn file_only_cfg(path: &std::path::Path) -> SessionLog {
    SessionLog {
        file: Some(llmenv_config::FileSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            path: Some(path.to_string_lossy().into_owned()),
        }),
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: false,
            level: LogLevel::Info,
            retention_days: None,
        }),
        max_content_bytes: None,
    }
}

fn jsonl_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn session_start_file_only_writes_lifecycle_and_scope() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    handle_session_log(
        HookEvent::SessionStart,
        &file_only_cfg(&path),
        None,
        None,
        &ctx(),
        None,
    )
    .await;
    let lines = jsonl_lines(&path);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["kind"], "lifecycle_start");
    assert_eq!(lines[1]["kind"], "scope");
    assert!(
        lines[1]["content"]
            .as_str()
            .unwrap()
            .contains("llmenv-tag:rust")
    );
}

#[tokio::test]
async fn session_end_file_only_writes_lifecycle_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    handle_session_log(
        HookEvent::SessionEnd,
        &file_only_cfg(&path),
        None,
        None,
        &ctx(),
        None,
    )
    .await;
    let lines = jsonl_lines(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["kind"], "lifecycle_end");
}

#[tokio::test]
async fn disabled_sinks_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let cfg = SessionLog {
        file: Some(llmenv_config::FileSinkConfig {
            enabled: false,
            level: LogLevel::Info,
            path: Some(path.to_string_lossy().into_owned()),
        }),
        transcript: Some(llmenv_config::TranscriptSinkConfig {
            enabled: false,
            level: LogLevel::Info,
            retention_days: None,
        }),
        max_content_bytes: None,
    };
    handle_session_log(HookEvent::SessionStart, &cfg, None, None, &ctx(), None).await;
    assert!(!path.exists());
}

fn mock_text_response(text: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":1,
        "result":{"content":[{"type":"text","text":text}]}})
}

// These two test `ensure_transcript_session` directly rather than through
// `handle_session_log`/`emit_session_log`: since T11, the transcript
// *record* path dispatches via a detached child process
// (`session_log::detached::spawn_record`), which a unit test must not
// trigger (the test binary is not the `llmenv` binary `spawn_record`
// expects to re-invoke). `start_session` stays synchronous/inline
// (`ensure_transcript_session`), so it remains directly unit-testable.

#[tokio::test]
async fn ensure_transcript_session_starts_no_second_transcript_while_one_is_pending() {
    // A start whose link was not recorded leaves its pending marker. A later event must not
    // start another transcript, which would be a second orphan (#2834).
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    let seen = state::Link::Missing;
    let now = super::session_state::unix_now();
    assert!(state::claim_pending_at(&state_path, "claude-p", &seen, now).unwrap());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
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
        "claude-p",
        &ctx(),
        Some(&state_path),
        true,
    )
    .await;

    assert_eq!(id, None);
    server.verify().await;
}

#[tokio::test]
async fn ensure_transcript_session_creates_and_correlates_when_none_recorded() {
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("icm-sess-1")))
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
        "claude-1",
        &ctx(),
        Some(&state_path),
        true,
    )
    .await;

    assert_eq!(id.as_deref(), Some("icm-sess-1"));
    assert_eq!(
        state::lookup_session_at(&state_path, "claude-1")
            .unwrap()
            .as_deref(),
        Some("icm-sess-1")
    );
}

#[tokio::test]
async fn ensure_transcript_session_reuses_existing_after_verifying_it_is_live() {
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("transcript-sessions.json");
    state::record_session_at(&state_path, "claude-2", "icm-sess-2").unwrap();
    let server = MockServer::start().await;
    // Only `initialize` and the `icm_transcript_show` verification call
    // are mocked. If `ensure_transcript_session` fell through to
    // `start_session` instead of trusting a verified cached id, that
    // call would hit an unmocked request and the id assertion below
    // would fail.
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains(
            crate::session_log::transcript::SHOW_TOOL,
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_text_response("[]")))
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
        "claude-2",
        &ctx(),
        Some(&state_path),
        true,
    )
    .await;

    assert_eq!(id.as_deref(), Some("icm-sess-2"));
}

/// #1090 regression: `run_session_log`'s per-event path calls
/// `ensure_transcript_session` for every mapped hook event, not just
/// `SessionStart` — verifying on every one of those would turn each
/// logged tool call into an `icm_transcript_show` round trip that grows
/// with the transcript itself (`icm_transcript_show` has no cheap
/// existence-only form). `verify: false` must reuse a cached id without
/// ever calling ICM.
mod second;
