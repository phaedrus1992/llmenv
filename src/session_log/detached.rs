//! Detaches the per-event ICM transcript `record` MCP call into a background
//! child process so a hook invocation returns immediately instead of blocking
//! on the network round trip. `spawn_record` is the parent-side launcher
//! (called from `hook_run::emit_session_log`); `run_record` is the child
//! entrypoint, wired to the hidden `llmenv session-log-record` command.
//!
//! `start_session` (which must return an id the caller persists) stays
//! synchronous in the SessionStart hook — only the per-event records, which
//! fire on every turn, are detached.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::hook_run::mcp_client::McpHttpClient;
use crate::session_log::dispatch;
use crate::session_log::event::SessionLogEvent;

/// Per-call network timeout for the detached child's transcript record call.
const RECORD_TIMEOUT: Duration = Duration::from_secs(5);

/// The detached child's stdin payload: session id + event, as one JSON object
/// (rather than passing `session_id` as a CLI argument, which would be
/// visible to any local user via `ps`/`/proc/<pid>/cmdline` for the life of
/// the child).
#[derive(Serialize, Deserialize)]
struct RecordPayload {
    session_id: String,
    event: SessionLogEvent,
}

/// Spawn a detached child that records `ev` into transcript session
/// `session_id`, then return immediately without waiting on it. The session
/// id and event are serialized to one JSON object and piped to the child's
/// stdin. Fail-soft: a spawn or serialization failure is logged and dropped,
/// mirroring every other session-log sink. The child's stderr goes to the
/// shared bounded log rather than `/dev/null` so its own failures are
/// diagnosable (#1133).
pub fn spawn_record(session_id: &str, ev: &SessionLogEvent) {
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("session_log: cannot resolve current_exe for detached record");
        return;
    };
    let payload = RecordPayload {
        session_id: session_id.to_string(),
        event: ev.clone(),
    };
    let Ok(payload_json) = serde_json::to_string(&payload) else {
        tracing::debug!("session_log: cannot serialize event for detached record");
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("session-log-record")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    crate::hook_run::redirect_stderr_to_detached_log(&mut cmd);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::debug!("session_log: failed to spawn detached record child");
        return;
    };
    if let Some(mut stdin) = child.stdin.take()
        // Small, already-truncated payload: this write fits the pipe buffer
        // and completes without the child having read anything yet.
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        tracing::debug!("session_log: failed to pipe event to detached child: {e}");
    }
    // Not waited on: the child is process-group-detached and outlives us.
}

/// Child entrypoint: parse the `{session_id, event}` stdin payload, resolve
/// the active memory backend the same way a hook process would, and record
/// the event. There's no terminal to write to, so on error this logs via
/// `tracing::error!` and the parent (`spawn_record`) points the child's stderr
/// at a bounded log — `error!` rather than `warn!` because the default
/// `EnvFilter` (`RUST_LOG` unset) is ERROR-only and dropped the warning before
/// it could reach that log (#1133). When `session_log.file` is on, the event
/// also reaches the operator through the internal-ops `FileLogLayer` wired in
/// `main.rs`.
///
/// # Errors
/// Malformed payload, no active memory backend, an invalid backend URL, or
/// the MCP call itself failing.
pub fn run_record(payload_json: &str) -> anyhow::Result<()> {
    run_record_inner(payload_json).inspect_err(|e| {
        tracing::error!("session_log: detached record failed: {e}");
    })
}

fn run_record_inner(payload_json: &str) -> anyhow::Result<()> {
    let payload: RecordPayload = serde_json::from_str(payload_json)?;

    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config);
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?.into_url()?;
    let client = McpHttpClient::new(url, RECORD_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(dispatch::record(
        &client,
        &payload.session_id,
        &payload.event,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::event::{EventKind, EventScope};

    fn ev() -> SessionLogEvent {
        SessionLogEvent {
            ts: "t".into(),
            kind: EventKind::Scope,
            scope: EventScope::AgentSession,
            role: "system".into(),
            tool_name: None,
            tokens: None,
            level: None,
            content: "hi".into(),
            fields: serde_json::json!({}),
            trace_fields: None,
        }
    }

    #[test]
    fn spawn_record_returns_immediately_without_panicking() {
        // The child (re-invoking the current, test-harness executable with
        // args it doesn't understand) is expected to exit non-zero almost
        // instantly; spawn_record never waits on it, so this call itself must
        // return promptly regardless of what the child does. Use a generous
        // 5-second timeout to tolerate high parallel test load while still
        // catching any actual blocking behavior.
        let start = std::time::Instant::now();
        spawn_record("sess-1", &ev());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "spawn_record must not block on the child"
        );
    }

    // #1133: this child's only report channel is its (now log-redirected)
    // stderr, and the default `EnvFilter` with `RUST_LOG` unset is ERROR-only —
    // a `warn!` here was dropped before it could reach that log. Perversely,
    // this is the one child that could otherwise have recorded the failure.
    //
    // The malformed-payload rejection is asserted in the same test on purpose:
    // `tracing` caches a callsite's interest globally on first hit, so a
    // sibling test reaching this `error!` outside any subscriber would make the
    // capture order-dependent.
    #[test]
    fn run_record_rejects_malformed_payload_json_and_logs_at_error_level() {
        use tracing_subscriber::prelude::*;

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.jsonl");
        let sub = tracing_subscriber::registry().with(
            crate::session_log::tracing_layer::FileLogLayer::new(
                crate::session_log::file_sink::FileSink::new(log.clone()),
            )
            .with_filter(tracing_subscriber::filter::LevelFilter::ERROR),
        );
        let err = tracing::subscriber::with_default(sub, || run_record("not json").unwrap_err());

        assert!(err.to_string().to_lowercase().contains("expected"));
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            body.contains("detached record failed"),
            "the failure must log at a level the default EnvFilter passes: {body}"
        );
    }

    #[test]
    fn record_payload_roundtrips_session_id_and_event() {
        let payload = RecordPayload {
            session_id: "sess-1".to_string(),
            event: ev(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecordPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.event, ev());
    }
}
