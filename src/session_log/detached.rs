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
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::hook_run::idempotency::{self, Guard};
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
    /// Derived from the event so a re-spawn or a resumed job sends the same id (#2397). Empty in
    /// a payload from an older llmenv: the child then records without a guard.
    #[serde(default)]
    request_id: String,
}

/// The id of one transcript record: the same transcript, time, kind, and content give the same id.
fn record_request_id(session_id: &str, ev: &SessionLogEvent) -> String {
    let kind = format!("{:?}", ev.kind);
    idempotency::request_id(&["session-log-record", session_id, &ev.ts, &kind, &ev.content])
}

/// `ev` with the request id added to its `fields`, which become the record's `metadata`, so the id
/// is visible in ICM. An event whose fields are not an object is returned unchanged.
fn with_request_id(ev: &SessionLogEvent, id: &str) -> SessionLogEvent {
    let mut ev = ev.clone();
    if !id.is_empty()
        && let Some(fields) = ev.fields.as_object_mut()
    {
        fields.insert("request_id".into(), serde_json::json!(id));
    }
    ev
}

/// Spawn a detached child that records `ev` into transcript session
/// `session_id`, then return immediately without waiting on it. The session
/// id and event are serialized to one JSON object and piped to the child's
/// stdin. Fail-soft: a spawn or serialization failure is logged and dropped,
/// mirroring every other session-log sink. The child's stderr goes to the
/// shared bounded log rather than `/dev/null` so its own failures are
/// diagnosable (#1133).
///
/// Returns the spawned [`Child`] purely so callers such as tests can reap it
/// (#1095) — production intentionally drops it unwaited, identical to the
/// previous behavior, since the child is process-group-detached and outlives
/// this process regardless.
pub(crate) fn spawn_record(session_id: &str, ev: &SessionLogEvent) -> Option<Child> {
    let Ok(exe) = std::env::current_exe() else {
        tracing::debug!("session_log: cannot resolve current_exe for detached record");
        return None;
    };
    let payload = RecordPayload {
        session_id: session_id.to_string(),
        event: ev.clone(),
        request_id: record_request_id(session_id, ev),
    };
    let Ok(payload_json) = serde_json::to_string(&payload) else {
        tracing::debug!("session_log: cannot serialize event for detached record");
        return None;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("session-log-record")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    crate::hook_run::redirect_stderr_to_detached_log(&mut cmd);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::debug!("session_log: failed to spawn detached record child");
        return None;
    };
    if let Some(mut stdin) = child.stdin.take()
        // Small, already-truncated payload: this write fits the pipe buffer
        // and completes without the child having read anything yet.
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        tracing::debug!("session_log: failed to pipe event to detached child: {e}");
    }
    // Not waited on by the caller: the child is process-group-detached and
    // outlives us.
    Some(child)
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
pub(crate) fn run_record(payload_json: &str) -> anyhow::Result<()> {
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
    let state_dir = crate::paths::state_dir().ok();
    rt.block_on(record_once(&client, state_dir.as_deref(), &payload))
}

/// Record the event unless its request id was already recorded, and record the id after the call
/// succeeds.
async fn record_once(
    client: &McpHttpClient,
    state_dir: Option<&std::path::Path>,
    payload: &RecordPayload,
) -> anyhow::Result<()> {
    let guard = Guard::new(state_dir, &payload.session_id, &payload.request_id);
    if guard.as_ref().is_some_and(Guard::already_done) {
        return Ok(());
    }
    let event = with_request_id(&payload.event, &payload.request_id);
    dispatch::record(client, &payload.session_id, &event).await?;
    if let Some(guard) = &guard {
        guard.done();
    }
    Ok(())
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
        let child = spawn_record("sess-1", &ev());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "spawn_record must not block on the child"
        );
        // Reap: production deliberately never waits (the child is
        // process-group-detached), but this test process is still its OS
        // parent — leaving it un-waited leaks a zombie for the rest of the
        // cargo-test run (#1095).
        if let Some(mut child) = child {
            reap(&mut child, std::time::Duration::from_secs(5));
        }
    }

    /// Wait for `child` to exit, bounded by `timeout`; force-kill and wait
    /// again if it doesn't exit in time. Used only to keep test-spawned
    /// children from outliving the test run (#1095).
    fn reap(child: &mut std::process::Child, timeout: std::time::Duration) {
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if start.elapsed() < timeout => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
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
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.jsonl");
        let err = crate::session_log::tracing_layer::capture_file_logs_at(
            &log,
            tracing_subscriber::filter::LevelFilter::ERROR,
            || run_record("not json").unwrap_err(),
        );

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
            request_id: "abc".to_string(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecordPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.event, ev());
        assert_eq!(back.request_id, "abc");
    }

    #[test]
    fn a_payload_from_an_older_llmenv_parses_with_an_empty_request_id() {
        let json = serde_json::json!({"session_id": "s", "event": ev()}).to_string();
        let back: RecordPayload = serde_json::from_str(&json).unwrap();
        assert!(back.request_id.is_empty());
    }

    #[test]
    fn record_request_id_follows_session_time_kind_and_content() {
        let base = record_request_id("s1", &ev());
        assert_eq!(base, record_request_id("s1", &ev()));
        assert_ne!(base, record_request_id("s2", &ev()));
        let mut later = ev();
        later.ts = "t2".into();
        assert_ne!(base, record_request_id("s1", &later));
        let mut other = ev();
        other.content = "bye".into();
        assert_ne!(base, record_request_id("s1", &other));
    }

    #[test]
    fn the_request_id_lands_in_the_record_metadata() {
        let with = with_request_id(&ev(), "abc");
        let args = crate::session_log::transcript::record_args("s", &with);
        assert!(
            args["metadata"]
                .as_str()
                .unwrap()
                .contains("\"request_id\":\"abc\"")
        );
        let none = with_request_id(&ev(), "");
        assert!(none.fields.get("request_id").is_none());
    }

    fn ok_body() -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":"ok"}]}})
    }

    async fn tool_calls(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .filter(|b| b["method"] == "tools/call")
            .count()
    }

    fn payload_with_id(id: &str) -> RecordPayload {
        RecordPayload {
            session_id: "01KXE1FNZCF1A0EAHK5X207RBW".to_string(),
            event: ev(),
            request_id: id.to_string(),
        }
    }

    #[tokio::test]
    async fn the_same_payload_records_once() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        record_once(&client, Some(dir.path()), &payload_with_id("abc"))
            .await
            .unwrap();
        record_once(&client, Some(dir.path()), &payload_with_id("abc"))
            .await
            .unwrap();
        assert_eq!(tool_calls(&server).await, 1);
    }

    #[tokio::test]
    async fn a_failed_record_is_retried() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_string_contains("tools/call"))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(1)
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(
            record_once(&client, Some(dir.path()), &payload_with_id("abc"))
                .await
                .is_err()
        );
        assert!(
            record_once(&client, Some(dir.path()), &payload_with_id("abc"))
                .await
                .is_err()
        );
        assert_eq!(tool_calls(&server).await, 2);
    }
}
