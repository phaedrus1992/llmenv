//! Issues the two transcript MCP calls through the shared `McpHttpClient`.
//! Callers run these inside a current-thread tokio runtime (see `hook_run`).

use serde_json::Value;

use crate::hook_run::mcp_client::McpHttpClient;
use crate::session_log::event::SessionLogEvent;
use crate::session_log::transcript::{RECORD_TOOL, START_TOOL, record_args, start_session_args};

/// Start a transcript session; returns its id (the tool's text result, trimmed).
///
/// # Errors
/// Any `call_tool` failure, an empty id, or an id containing whitespace/control
/// characters (#509 item 1: defense-in-depth — the id is persisted to
/// `transcript-sessions.json` and later passed as a CLI/process argument
/// elsewhere, so a well-formed id matters even though ICM is a trusted
/// boundary today).
pub async fn start_session(
    client: &McpHttpClient,
    agent: &str,
    project: Option<&str>,
    metadata: &Value,
) -> anyhow::Result<String> {
    let text = client
        .call_tool(START_TOOL, start_session_args(agent, project, metadata))
        .await?;
    let id = text.trim().to_string();
    if id.is_empty() {
        anyhow::bail!("{START_TOOL} returned an empty session id");
    }
    if id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!(
            "{START_TOOL} returned a session id with whitespace/control characters: {id:?}"
        );
    }
    Ok(id)
}

/// Record one event into `session_id`.
///
/// # Errors
/// Any `call_tool` failure.
pub async fn record(
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
    async fn start_session_rejects_id_with_whitespace() {
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
        assert!(err.to_string().contains("whitespace/control"));
    }

    #[tokio::test]
    async fn start_session_rejects_id_with_control_character() {
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
        assert!(err.to_string().contains("whitespace/control"));
    }

    #[tokio::test]
    async fn record_posts_without_error() {
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
        };
        record(&client, "sess-42", &ev).await.unwrap();
    }
}
