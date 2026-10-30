//! Mirrors llmenv's own `tracing` events (materialization, change detection,
//! etc.) into the session-log file sink as `kind=internal`, `scope=process`
//! events — this is the diagnostic value the pre-3.0 `session_log` file had.
//! Never touches the transcript sink: internal events have no agent session.

use std::cell::Cell;

use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::session_log::event::{EventKind, EventScope, SessionLogEvent, now_rfc3339};
use crate::session_log::file_sink::FileSink;
use crate::util::display_safe;

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
            // A message can interpolate an attacker-influenced string (a file
            // path, a rejected tag/bundle name); escape control chars so a
            // tool that later prints `content` raw (e.g. `jq -r`) can't have
            // its terminal output rewritten.
            self.message = display_safe(&format!("{value:?}")).into_owned();
        }
    }
}

thread_local! {
    static IN_ON_EVENT: Cell<bool> = const { Cell::new(false) };
}

/// Clears the re-entry flag when `on_event` returns, also on a panic.
struct ReentryGuard;

impl ReentryGuard {
    /// `None` when this thread is already inside `on_event`.
    fn enter() -> Option<Self> {
        IN_ON_EVENT.with(|flag| (!flag.replace(true)).then_some(Self))
    }
}

impl Drop for ReentryGuard {
    fn drop(&mut self) {
        IN_ON_EVENT.with(|flag| flag.set(false));
    }
}

impl<S: Subscriber> Layer<S> for FileLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > Level::INFO {
            return;
        }
        // A failed append emits an event of its own. The global dispatcher sends that event
        // back here, and with no guard the recursion overflows the stack (#3136).
        let Some(_guard) = ReentryGuard::enter() else {
            return;
        };
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
    fn control_char_in_message_is_escaped() {
        // A tracing call can interpolate an attacker-influenced string (a
        // file path, a rejected tag/bundle name). Unescaped, a control char
        // survives the JSON round trip and can rewrite a terminal when a
        // tool like `jq -r` prints `content` raw.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        capture_file_logs_at(
            &path,
            tracing_subscriber::filter::LevelFilter::TRACE,
            || {
                tracing::info!(target: "llmenv::test", "value is {}", "\x1b[31mred\x1b[0m");
            },
        );
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(
            !content.contains('\x1b'),
            "raw ESC must not survive into content: {content}"
        );
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

// #3136: an append that keeps failing must not recurse through the layer.
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod reentry_tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    // The global dispatcher has no re-entry guard, but a scoped `with_default` one has.
    // `main.rs` installs the layer globally, so the test must too. nextest runs each test
    // in its own process, so the global default does not leak into other tests.
    #[test]
    fn a_failing_append_through_the_global_layer_returns() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.jsonl");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let sub = tracing_subscriber::registry().with(FileLogLayer::new(FileSink::new(link)));
        tracing::subscriber::set_global_default(sub).unwrap();
        tracing::error!("first event");
        tracing::error!("second event");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }
}
