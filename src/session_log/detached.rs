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

use crate::hook_run::mcp_client::McpHttpClient;
use crate::session_log::dispatch;
use crate::session_log::event::SessionLogEvent;

/// Per-call network timeout for the detached child's transcript record call.
const RECORD_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn a detached child that records `ev` into transcript session
/// `session_id`, then return immediately without waiting on it. The event is
/// serialized to JSON and piped to the child's stdin. Fail-soft: a spawn or
/// serialization failure is logged and dropped, mirroring every other
/// session-log sink.
pub fn spawn_record(session_id: &str, ev: &SessionLogEvent) {
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("session_log: cannot resolve current_exe for detached record");
        return;
    };
    let Ok(event_json) = serde_json::to_string(ev) else {
        tracing::debug!("session_log: cannot serialize event for detached record");
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("session-log-record")
        .arg("--session")
        .arg(session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::debug!("session_log: failed to spawn detached record child");
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // Small, already-truncated payload: this write fits the pipe buffer
        // and completes without the child having read anything yet.
        let _ = stdin.write_all(event_json.as_bytes());
    }
    // Not waited on: the child is process-group-detached and outlives us.
}

/// Child entrypoint: parse `event_json`, resolve the active memory backend the
/// same way a hook process would, and record the event into `session_id`.
///
/// # Errors
/// Malformed `event_json`, no active memory backend, an invalid backend URL,
/// or the MCP call itself failing.
pub fn run_record(session_id: &str, event_json: &str) -> anyhow::Result<()> {
    let ev: SessionLogEvent = serde_json::from_str(event_json)?;

    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;
    let client = McpHttpClient::new(url, RECORD_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(dispatch::record(&client, session_id, &ev))
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
        }
    }

    #[test]
    fn spawn_record_returns_immediately_without_panicking() {
        // The child (re-invoking the current, test-harness executable with
        // args it doesn't understand) is expected to exit non-zero almost
        // instantly; spawn_record never waits on it, so this call itself must
        // return promptly regardless of what the child does.
        let start = std::time::Instant::now();
        spawn_record("sess-1", &ev());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "spawn_record must not block on the child"
        );
    }

    #[test]
    fn run_record_rejects_malformed_event_json() {
        let err = run_record("sess-1", "not json").unwrap_err();
        assert!(err.to_string().to_lowercase().contains("expected"));
    }
}
