//! Maps session-log events to ICM transcript MCP tool-call arguments. The calls
//! themselves go through `McpHttpClient` (see `dispatch.rs`); these are pure
//! argument builders so they are unit-testable without a server.

use serde_json::{Value, json};

use crate::session_log::event::SessionLogEvent;

/// ICM MCP tool: create a transcript session, returns its id as text.
pub const START_TOOL: &str = "icm_transcript_start_session";
/// ICM MCP tool: append one message to a session.
pub const RECORD_TOOL: &str = "icm_transcript_record";

/// Arguments for `icm_transcript_start_session`.
#[must_use]
pub fn start_session_args(agent: &str, project: Option<&str>, metadata: &Value) -> Value {
    json!({
        "agent": agent,
        "project": project,
        // ICM stores session metadata as a JSON string.
        "metadata": metadata.to_string(),
    })
}

/// Arguments for `icm_transcript_record` for `ev` in session `session_id`.
#[must_use]
pub fn record_args(session_id: &str, ev: &SessionLogEvent) -> Value {
    let mut a = json!({
        "session_id": session_id,
        "role": ev.role,
        "content": ev.content,
        "metadata": ev.fields.to_string(),
    });
    if let Some(t) = &ev.tool_name {
        a["tool_name"] = json!(t);
    }
    if let Some(n) = ev.tokens {
        a["tokens"] = json!(n);
    }
    a
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::event::{EventKind, EventScope, SessionLogEvent};

    fn ev() -> SessionLogEvent {
        SessionLogEvent {
            ts: "t".into(),
            kind: EventKind::ToolUse,
            scope: EventScope::AgentSession,
            role: "tool".into(),
            tool_name: Some("Bash".into()),
            tokens: Some(5),
            level: None,
            content: "ls".into(),
            fields: serde_json::json!({"k":1}),
            trace_fields: None,
        }
    }

    #[test]
    fn start_session_args_shape() {
        let a = start_session_args(
            "claude_code",
            Some("llmenv"),
            &serde_json::json!({"tags":["rust"]}),
        );
        assert_eq!(a["agent"], "claude_code");
        assert_eq!(a["project"], "llmenv");
        // metadata is a JSON string, not a nested object.
        assert!(a["metadata"].as_str().unwrap().contains("\"tags\""));
    }

    #[test]
    fn record_args_map_event_fields() {
        let a = record_args("sess-1", &ev());
        assert_eq!(a["session_id"], "sess-1");
        assert_eq!(a["role"], "tool");
        assert_eq!(a["content"], "ls");
        assert_eq!(a["tool_name"], "Bash");
        assert_eq!(a["tokens"], 5);
        assert!(a["metadata"].as_str().unwrap().contains("\"k\":1"));
    }
}
