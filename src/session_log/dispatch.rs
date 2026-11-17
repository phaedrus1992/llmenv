//! Issues the two transcript MCP calls through the shared `McpHttpClient`.
//! Callers run these inside a current-thread tokio runtime (see `hook_run`).

use serde_json::Value;

use crate::session_log::event::SessionLogEvent;
use crate::session_log::transcript::{
    RECORD_TOOL, SHOW_TOOL, START_TOOL, record_args, show_session_args, start_session_args,
};
use llmenv_mcp::mcp_client::McpHttpClient;

/// Start a transcript session; returns its id (the tool's text result, trimmed).
///
/// # Errors
/// Any `call_tool` failure, an empty id, or a non-ASCII id (#509 item 1:
/// defense-in-depth — the id is persisted to `transcript-sessions.json` and
/// later passed as a CLI/process argument elsewhere, so a well-formed id
/// matters even though ICM is a trusted boundary today). Every valid session
/// id (UUID, ULID) is pure ASCII, so rejecting anything else — not just
/// `is_whitespace()`/`is_control()`, which miss Unicode formatting characters
/// like zero-width space or RTL override — closes the gap with no false
/// positives.
pub(crate) async fn start_session(
    client: &McpHttpClient,
    agent: &str,
    project: Option<&str>,
    metadata: &Value,
) -> anyhow::Result<String> {
    let text = client
        .call_tool(START_TOOL, start_session_args(agent, project, metadata))
        .await?;
    let id = session_id_from_reply(text.trim())?;
    if id.is_empty() {
        anyhow::bail!("{START_TOOL} returned an empty session id");
    }
    if !id.is_ascii() || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("{START_TOOL} returned a malformed session id: {id:?}");
    }
    Ok(id)
}

/// The session id in the reply of `icm_transcript_start_session`. ICM returns a JSON object like
/// `{"session_id":"01KXE..."}`, and it appends a hint line such as `[ICM: 10 tool calls since
/// last store...]` to a reply once a session has made enough calls without a store (#3754). The
/// first JSON value is read and whatever follows it is ignored. A reply that is not JSON is a bare
/// id, for forward compatibility with the backend: its first line is the id.
///
/// # Errors
/// The reply is a JSON object with no string `session_id`, or it starts with `{` or `[` and is
/// not a complete JSON object.
fn session_id_from_reply(raw: &str) -> anyhow::Result<String> {
    match serde_json::Deserializer::from_str(raw)
        .into_iter::<Value>()
        .next()
    {
        Some(Ok(Value::Object(object))) => object
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{START_TOOL} returned a reply with no `session_id` string: {raw:?}"
                )
            }),
        // A reply that starts like JSON but is not an object is a damaged reply. It is an error,
        // because the first line would become a wrong id that fails later, far from the cause.
        _ if raw.starts_with(['{', '[']) => Err(anyhow::anyhow!(
            "{START_TOOL} returned a reply that is not a JSON object with a `session_id`: {raw:?}"
        )),
        _ => Ok(raw.lines().next().unwrap_or_default().trim().to_string()),
    }
}

/// The text ICM puts in its tool error when `session_id` is unknown. ICM has no typed
/// not-found code, so this text is the only signal. The id is part of the text: ICM
/// sends `session <id> not found` (icm-mcp `tool_transcript_show`).
fn session_gone_text(session_id: &str) -> String {
    format!("session {session_id} not found")
}

/// Whether ICM still holds a recorded transcript session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// ICM returned the session.
    Live,
    /// ICM answered that the session does not exist.
    Gone,
}

/// Ask ICM whether `session_id` still exists.
///
/// A recorded session id is a cache, not proof of liveness (#1090). ICM may
/// have pruned or restarted since it was recorded.
///
/// # Errors
/// A transport failure, or any reply that is not "session <id> not found". ICM being
/// unreachable says nothing about the session, so the caller keeps its link then (#2667).
pub(crate) async fn verify_session(
    client: &McpHttpClient,
    session_id: &str,
) -> anyhow::Result<Liveness> {
    match client
        .call_tool(SHOW_TOOL, show_session_args(session_id))
        .await
    {
        Ok(_) => Ok(Liveness::Live),
        Err(e) if format!("{e:#}").contains(&session_gone_text(session_id)) => Ok(Liveness::Gone),
        Err(e) => Err(e),
    }
}

/// Record one event into `session_id`.
///
/// # Errors
/// Any `call_tool` failure.
pub(crate) async fn record(
    client: &McpHttpClient,
    session_id: &str,
    ev: &SessionLogEvent,
) -> anyhow::Result<()> {
    client
        .call_tool(RECORD_TOOL, record_args(session_id, ev))
        .await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::event::{EventKind, EventScope, SessionLogEvent};
    use std::time::Duration;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn text_result(text: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":text}]}})
    }

    #[tokio::test]
    async fn start_session_parses_returned_id() {
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("sess-42")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let id = start_session(
            &client,
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(id, "sess-42");
    }

    #[tokio::test]
    async fn start_session_extracts_session_id_from_json_object() {
        // ICM returns `{"session_id":"..."}`, not a bare ULID.
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result(
                r#"{"session_id":"01KXE1FNZCF1A0EAHK5X207RBW"}"#,
            )))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let id = start_session(
            &client,
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(id, "01KXE1FNZCF1A0EAHK5X207RBW");
    }

    #[test]
    fn a_bare_id_keeps_only_its_first_line() {
        assert_eq!(
            session_id_from_reply("sess-42\n[ICM: 10 tool calls since last store.]").unwrap(),
            "sess-42"
        );
        assert_eq!(session_id_from_reply("12345").unwrap(), "12345");
        assert_eq!(session_id_from_reply("").unwrap(), "");
    }

    #[test]
    fn a_json_object_with_no_session_id_names_the_reply() {
        let err = session_id_from_reply(r#"{"id":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("no `session_id` string"), "{err}");
        let err = session_id_from_reply(r#"{"session_id":42}"#).unwrap_err();
        assert!(err.to_string().contains("no `session_id` string"), "{err}");
    }

    #[tokio::test]
    async fn start_session_accepts_a_reply_with_a_hint_line() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result(
                "{\"session_id\":\"01M7JV17C1MPXQ7DHRX184B2QD\"}\n[ICM: 550 tool calls since last store.]",
            )))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let id = start_session(
            &client,
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(id, "01M7JV17C1MPXQ7DHRX184B2QD");
    }

    #[test]
    fn a_damaged_json_reply_is_an_error_and_not_an_id() {
        for reply in [r#"{"session_id":"01KXE"#, r#"["01KXE"]"#, "{"] {
            let err = session_id_from_reply(reply).unwrap_err();
            assert!(
                err.to_string().contains("not a JSON object"),
                "{reply}: {err}"
            );
        }
    }

    proptest::proptest! {
        /// A reply that does not start like JSON is a bare id: its trimmed first line.
        #[test]
        fn a_bare_reply_gives_its_first_line(
            first in "[0-9A-Za-z_-][ -~]{0,30}",
            tail in "(\\n[ -~]{0,40}){0,3}",
        ) {
            let reply = format!("{first}{tail}");
            proptest::prop_assert_eq!(
                session_id_from_reply(&reply).unwrap(),
                first.trim().to_owned()
            );
        }

        /// An object with no string `session_id` is an error, whatever its other keys hold.
        #[test]
        fn an_object_without_a_string_id_is_an_error(
            key in "[a-z]{1,8}",
            number in 0u32..1000,
        ) {
            proptest::prop_assume!(key != "session_id");
            let reply = serde_json::json!({ key: number, "session_id": number }).to_string();
            proptest::prop_assert!(session_id_from_reply(&reply).is_err());
        }

        /// Whatever text follows the JSON object, the id is the one in the object.
        #[test]
        fn text_after_the_object_never_changes_the_id(
            id in "[0-9A-Z]{8,26}",
            tail in "(\\n[^\\x00]{0,60})?",
        ) {
            let reply = format!("{{\"session_id\":\"{id}\"}}{tail}");
            proptest::prop_assert_eq!(session_id_from_reply(&reply).unwrap(), id);
        }
    }

    #[tokio::test]
    async fn start_session_rejects_id_with_whitespace() {
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("sess 42")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let err = start_session(
            &client,
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("malformed session id"));
    }

    #[tokio::test]
    async fn start_session_rejects_id_with_control_character() {
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("sess\x0042")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let err = start_session(
            &client,
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("malformed session id"));
    }

    #[tokio::test]
    async fn start_session_rejects_non_ascii_id() {
        // Zero-width space (U+200B): not caught by is_whitespace()/is_control(),
        // but is_ascii() rejects it.
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("sess\u{200B}42")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let err = start_session(
            &client,
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("malformed session id"));
    }

    #[tokio::test]
    async fn record_posts_without_error() {
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("ok")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let ev = SessionLogEvent {
            ts: "t".into(),
            kind: EventKind::Prompt,
            scope: EventScope::AgentSession,
            role: "user".into(),
            tool_name: None,
            tokens: None,
            level: None,
            content: "hi".into(),
            fields: serde_json::json!({}),
            trace_fields: None,
        };
        record(&client, "sess-42", &ev).await.unwrap();
    }

    #[tokio::test]
    async fn verify_session_succeeds_when_icm_returns_the_session() {
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("[]")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        assert_eq!(
            verify_session(&client, "sess-42").await.unwrap(),
            Liveness::Live
        );
    }

    /// ICM reports a tool failure as a successful reply with `isError: true`.
    fn tool_error(text: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"isError":true,"content":[{"type":"text","text":text}]}})
    }

    async fn verify_with_tool_error(text: &str, session_id: &str) -> anyhow::Result<Liveness> {
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_error(text)))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        verify_session(&client, session_id).await
    }

    #[tokio::test]
    async fn verify_session_reports_gone_for_the_text_icm_really_sends() {
        // icm-mcp tool_transcript_show: format!("session {session_id} not found")
        let got = verify_with_tool_error("session stale-session not found", "stale-session").await;
        assert_eq!(got.unwrap(), Liveness::Gone);
    }

    #[tokio::test]
    async fn verify_session_keeps_the_link_for_any_other_tool_error() {
        let got = verify_with_tool_error("get_session failed: disk I/O error", "sess-1").await;
        assert!(got.is_err(), "another tool error must not read as gone");
    }

    #[tokio::test]
    async fn verify_session_ignores_a_not_found_for_a_different_session() {
        let got = verify_with_tool_error("session other-id not found", "sess-1").await;
        assert!(
            got.is_err(),
            "a not-found for another id must not read as gone"
        );
    }

    #[tokio::test]
    async fn verify_session_errors_when_icm_is_unreachable() {
        // A transport failure must not read as "gone": the caller keeps the link.
        let _guard = llmenv_util::testkit::port_guard();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        assert!(verify_session(&client, "sess-42").await.is_err());
    }
}
