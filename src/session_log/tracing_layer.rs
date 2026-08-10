//! Mirrors llmenv's own `tracing` events (materialization, change detection,
//! etc.) into the session-log file sink as `kind=internal`, `scope=process`
//! events — this is the diagnostic value the pre-3.0 `session_log` file had.
//! Never touches the transcript sink: internal events have no agent session.

use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::session_log::event::{EventKind, EventScope, SessionLogEvent, now_rfc3339};
use crate::session_log::file_sink::FileSink;

/// A `tracing_subscriber::Layer` that appends `info!`+ events to a `FileSink`.
#[derive(Debug)]
pub struct FileLogLayer {
    sink: FileSink,
}

impl FileLogLayer {
    #[must_use]
    pub fn new(sink: FileSink) -> Self {
        Self { sink }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for FileLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > Level::INFO {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let ev = SessionLogEvent {
            ts: now_rfc3339(),
            kind: EventKind::Internal,
            scope: EventScope::Process,
            role: "system".into(),
            tool_name: None,
            tokens: None,
            level: Some(meta.level().to_string()),
            content: visitor.message,
            fields: serde_json::json!({"target": meta.target()}),
            trace_fields: None,
        };
        self.sink.append(&ev.to_jsonl());
    }
}

/// Run `f` under a `tracing` subscriber that captures `level`-and-above
/// events into a `FileSink` at `log_path`.
///
/// Shared by the detached hook children's failure-logging tests
/// (`hook_run::detached_store`, `session_log::detached`, `hook_run`) so the
/// tempdir/`FileSink`/`FileLogLayer`/`with_default` scaffold — and which
/// level it filters at — can't drift between them (#1141). Each of those
/// tests asserts that a specific failure logs at a level the default
/// `EnvFilter` (ERROR-only with `RUST_LOG` unset) actually passes.
#[cfg(test)]
pub(crate) fn capture_file_logs_at<R>(
    log_path: &std::path::Path,
    level: tracing_subscriber::filter::LevelFilter,
    f: impl FnOnce() -> R,
) -> R {
    use tracing_subscriber::prelude::*;
    let sub = tracing_subscriber::registry()
        .with(FileLogLayer::new(FileSink::new(log_path.to_path_buf())).with_filter(level));
    tracing::subscriber::with_default(sub, f)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn info_event_is_written_as_internal_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        capture_file_logs_at(
            &path,
            tracing_subscriber::filter::LevelFilter::TRACE,
            || {
                tracing::info!(target: "llmenv::materialize", "materialized 3 files");
            },
        );
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(v["kind"], "internal");
        assert_eq!(v["scope"], "process");
        assert_eq!(v["level"], "INFO");
        assert!(
            v["content"]
                .as_str()
                .unwrap()
                .contains("materialized 3 files")
        );
        assert_eq!(v["fields"]["target"], "llmenv::materialize");
    }

    #[test]
    fn debug_event_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        capture_file_logs_at(
            &path,
            tracing_subscriber::filter::LevelFilter::TRACE,
            || {
                tracing::debug!("should not appear");
            },
        );
        assert!(!path.exists());
    }
}
