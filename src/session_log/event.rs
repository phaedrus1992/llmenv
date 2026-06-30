//! The single event type session logging produces, and how it renders to a
//! JSONL line. The transcript-record mapping lives in `transcript.rs`.

use serde::Serialize;

/// Whether an event belongs to a correlated agent transcript session or is a
/// process-level llmenv diagnostic (no session to attach to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// Part of a correlated agent session (created in the SessionStart hook).
    AgentSession,
    /// A process-level llmenv diagnostic with no transcript session.
    Process,
}

/// The kind of session event. Drives transcript `role`/`kind` labelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    LifecycleStart,
    Scope,
    Internal,
    Prompt,
    ToolUse,
    ToolResult,
    Notification,
    Stop,
    LifecycleEnd,
}

/// One session-logging event. Both sinks consume this; the file sink writes
/// `to_jsonl`, the transcript sink maps it via `transcript::record_args`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionLogEvent {
    /// RFC 3339 timestamp.
    pub ts: String,
    pub kind: EventKind,
    pub scope: EventScope,
    /// Transcript role: `user` | `assistant` | `system` | `tool`.
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Log level for `internal` events (e.g. `INFO`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Rendered, FTS-searchable content.
    pub content: String,
    /// Structured payload (tags, bundles, scopes, op name, …).
    pub fields: serde_json::Value,
}

impl SessionLogEvent {
    /// Render to a single JSONL line (no trailing newline).
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        // Serialize is infallible for this shape; fall back to a minimal line.
        serde_json::to_string(self)
            .unwrap_or_else(|_| format!("{{\"ts\":\"{}\",\"kind\":\"internal\"}}", self.ts))
    }

    /// Cap `content` to `max` bytes (on a char boundary), returning self.
    #[must_use]
    pub fn truncated(mut self, max: usize) -> Self {
        if self.content.len() > max {
            let mut end = max;
            while !self.content.is_char_boundary(end) {
                end -= 1;
            }
            self.content.truncate(end);
        }
        self
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ev() -> SessionLogEvent {
        SessionLogEvent {
            ts: "2026-06-30T00:00:00Z".into(),
            kind: EventKind::Prompt,
            scope: EventScope::AgentSession,
            role: "user".into(),
            tool_name: None,
            tokens: None,
            level: None,
            content: "hello".into(),
            fields: serde_json::json!({}),
        }
    }

    #[test]
    fn to_jsonl_is_one_line_with_required_fields() {
        let line = ev().to_jsonl();
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "prompt");
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hello");
        assert_eq!(v["ts"], "2026-06-30T00:00:00Z");
    }

    #[test]
    fn truncated_caps_content_bytes() {
        let mut e = ev();
        e.content = "x".repeat(100);
        let t = e.truncated(10);
        assert_eq!(t.content.len(), 10);
    }

    #[test]
    fn truncated_is_noop_when_within_cap() {
        let t = ev().truncated(1000);
        assert_eq!(t.content, "hello");
    }
}
