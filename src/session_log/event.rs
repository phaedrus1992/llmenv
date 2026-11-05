//! The single event type session logging produces, and how it renders to a
//! JSONL line. The transcript-record mapping lives in `transcript.rs`.

use llmenv_config::LogLevel;
use serde::{Deserialize, Serialize};

/// Whether an event belongs to a correlated agent transcript session or is a
/// process-level llmenv diagnostic (no session to attach to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// Part of a correlated agent session (created in the SessionStart hook).
    AgentSession,
    /// A process-level llmenv diagnostic with no transcript session.
    Process,
}

/// The kind of session event. Drives transcript `role`/`kind` labelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

impl EventKind {
    #[must_use]
    pub(crate) fn log_level(self) -> LogLevel {
        match self {
            EventKind::LifecycleStart
            | EventKind::LifecycleEnd
            | EventKind::Scope
            | EventKind::Prompt
            | EventKind::Notification
            | EventKind::Stop
            | EventKind::Internal => LogLevel::Info,
            EventKind::ToolUse | EventKind::ToolResult => LogLevel::Debug,
        }
    }
}

/// One session-logging event. Both sinks consume this; the file sink writes
/// `to_jsonl`, the transcript sink maps it via `transcript::record_args`.
/// Also round-trips through JSON when a hook process hands an event off to
/// the detached transcript-record child (`session_log::detached`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLogEvent {
    /// RFC 3339 timestamp.
    pub(crate) ts: String,
    pub(crate) kind: EventKind,
    pub(crate) scope: EventScope,
    /// Transcript role: `user` | `assistant` | `system` | `tool`.
    pub(crate) role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tokens: Option<u64>,
    /// Log level for `internal` events (e.g. `INFO`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) level: Option<String>,
    /// Rendered, FTS-searchable content.
    pub(crate) content: String,
    /// Structured payload (tags, bundles, scopes, op name, …).
    pub(crate) fields: serde_json::Value,
    /// Hook diagnostics for Trace-level events: stdout, stderr, exit code.
    /// Only populated when the event is at Trace level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) trace_fields: Option<serde_json::Value>,
}

/// Current time as an RFC 3339 timestamp (e.g. `2026-06-30T00:00:00Z`).
#[must_use]
pub(crate) fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

impl SessionLogEvent {
    /// Minimum level this event should be recorded at.
    #[must_use]
    pub(crate) fn log_level(&self) -> LogLevel {
        self.kind.log_level()
    }

    /// Render to a single JSONL line (no trailing newline).
    #[must_use]
    pub(crate) fn to_jsonl(&self) -> String {
        // Serialize cannot fail for this shape: every field is a string, an integer, an enum, or
        // a `serde_json::Value`. The round-trip property over arbitrary values proves it (#3390).
        // The fallback stays so a future field cannot panic or drop the line.
        serde_json::to_string(self)
            .inspect_err(|e| {
                tracing::error!(
                    error = %e,
                    kind = ?self.kind,
                    "failed to serialize SessionLogEvent — all data except ts lost in fallback"
                )
            })
            .unwrap_or_else(|_| format!("{{\"ts\":\"{}\",\"kind\":\"internal\"}}", self.ts))
    }

    /// Cap `content` to `max` bytes (on a char boundary), returning self.
    #[must_use]
    pub(crate) fn truncated(mut self, max: usize) -> Self {
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
            trace_fields: None,
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

    #[test]
    fn log_level_info_for_lifecycle_events() {
        assert_eq!(EventKind::LifecycleStart.log_level(), LogLevel::Info);
        assert_eq!(EventKind::LifecycleEnd.log_level(), LogLevel::Info);
        assert_eq!(EventKind::Scope.log_level(), LogLevel::Info);
        assert_eq!(EventKind::Prompt.log_level(), LogLevel::Info);
        assert_eq!(EventKind::Notification.log_level(), LogLevel::Info);
        assert_eq!(EventKind::Stop.log_level(), LogLevel::Info);
    }

    #[test]
    fn log_level_debug_for_tool_events() {
        assert_eq!(EventKind::ToolUse.log_level(), LogLevel::Debug);
        assert_eq!(EventKind::ToolResult.log_level(), LogLevel::Debug);
    }

    #[test]
    fn log_level_ordering_is_correct() {
        assert!(LogLevel::Trace > LogLevel::Debug);
        assert!(LogLevel::Debug > LogLevel::Info);
    }

    use proptest::prelude::*;

    fn arb_event_kind() -> impl Strategy<Value = EventKind> {
        prop_oneof![
            Just(EventKind::LifecycleStart),
            Just(EventKind::Scope),
            Just(EventKind::Internal),
            Just(EventKind::Prompt),
            Just(EventKind::ToolUse),
            Just(EventKind::ToolResult),
            Just(EventKind::Notification),
            Just(EventKind::Stop),
            Just(EventKind::LifecycleEnd),
        ]
    }

    fn arb_event_scope() -> impl Strategy<Value = EventScope> {
        prop_oneof![Just(EventScope::AgentSession), Just(EventScope::Process)]
    }

    /// Any JSON value without a float: `serde_json::Value` has only string keys, so it always
    /// serializes. A float is left out because serde_json can read a long float back one ulp off,
    /// which is not a lost line.
    fn arb_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::from),
            any::<i64>().prop_map(serde_json::Value::from),
            ".{0,12}".prop_map(serde_json::Value::from),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::from),
                prop::collection::btree_map(".{0,8}", inner, 0..4)
                    .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
            ]
        })
    }

    fn arb_event() -> impl Strategy<Value = SessionLogEvent> {
        (
            (
                ".{0,30}",
                arb_json(),
                // JSON has no `Some(null)`, so it reads back as `None`. Producers never write it.
                prop::option::of(arb_json().prop_filter("not null", |v| !v.is_null())),
                arb_event_kind(),
                arb_event_scope(),
                "[a-z]{1,10}",
                prop::option::of("[a-zA-Z]{1,16}"),
            ),
            (
                prop::option::of(0u64..1_000_000),
                prop::option::of("[A-Z]{3,5}"),
                // `.` under regex-syntax matches any Unicode scalar value (incl.
                // multi-byte UTF-8) except newline, so this exercises char
                // boundaries beyond plain ASCII.
                ".{0,40}",
            ),
        )
            .prop_map(
                |(
                    (ts, fields, trace_fields, kind, scope, role, tool_name),
                    (tokens, level, content),
                )| {
                    SessionLogEvent {
                        ts,
                        kind,
                        scope,
                        role,
                        tool_name,
                        tokens,
                        level,
                        content,
                        fields,
                        trace_fields,
                    }
                },
            )
    }

    proptest! {
        /// `truncated` must always leave valid UTF-8 within the byte cap, and
        /// must not trim further than the one multi-byte char that crossed
        /// the boundary (the IPC payload to the detached record child and the
        /// JSONL file both depend on this never producing a half-char).
        #[test]
        fn truncated_is_valid_utf8_within_cap(ev in arb_event(), max in 0usize..80) {
            let original = ev.content.clone();
            let t = ev.truncated(max);
            prop_assert!(std::str::from_utf8(t.content.as_bytes()).is_ok());
            prop_assert!(t.content.len() <= max);
            if let Some(c) = original[t.content.len()..].chars().next() {
                prop_assert!(t.content.len() + c.len_utf8() > max);
            }
        }

        /// `SessionLogEvent` round-trips through JSON exactly: the detached
        /// record child (`session_log::detached::run_record`) deserializes
        /// the event a hook process serialized, so any field that doesn't
        /// survive the round-trip silently corrupts a transcript record.
        #[test]
        fn session_log_event_roundtrips_through_jsonl(ev in arb_event()) {
            // The fallback line has only `ts` and `kind`, so a round trip that matches proves the
            // fallback was not taken (#3390).
            let json = ev.to_jsonl();
            let back: SessionLogEvent = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(ev, back);
        }
    }
}
