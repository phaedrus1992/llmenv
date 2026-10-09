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

use crate::hook_run::checkpoint::{self, JobKind};
use crate::hook_run::idempotency::{self, Guard};
use crate::session_log::dispatch;
use crate::session_log::event::SessionLogEvent;
use llmenv_mcp::mcp_client::McpHttpClient;

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

/// The id of one transcript record. `run_tag` is new for each event: two events with the same kind
/// and text inside one second are two records, so the time alone cannot tell them apart. The id
/// travels in the payload, so a re-spawn or a resume of the same event sends the same id.
fn record_request_id(session_id: &str, ev: &SessionLogEvent, run_tag: &str) -> String {
    let kind = format!("{:?}", ev.kind);
    idempotency::request_id(&[
        "session-log-record",
        session_id,
        &ev.ts,
        &kind,
        &ev.content,
        run_tag,
    ])
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
    spawn_record_in(checkpoint::spawner_state_dir().as_deref(), session_id, ev)
}

/// [`spawn_record`] with the state dir for the checkpoint given (#2396).
fn spawn_record_in(
    state_dir: Option<&std::path::Path>,
    session_id: &str,
    ev: &SessionLogEvent,
) -> Option<Child> {
    // Spawns this same binary as a detached child. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let Ok(exe) = std::env::current_exe() else {
        tracing::error!("session_log: cannot resolve current_exe for detached record");
        return None;
    };
    let payload = RecordPayload {
        session_id: session_id.to_string(),
        event: ev.clone(),
        request_id: record_request_id(session_id, ev, &checkpoint::run_tag()),
    };
    let Ok(payload_json) = serde_json::to_string(&payload) else {
        tracing::error!("session_log: cannot serialize event for detached record");
        return None;
    };
    let checkpoint = checkpoint::begin(
        state_dir,
        JobKind::SessionLogRecord,
        &payload,
        Some(session_id),
    );
    let mut cmd = record_command(exe, checkpoint.as_deref());
    crate::session_log::redirect_stderr_to_detached_log(
        &mut cmd,
        crate::session_log::detached_child_log_path,
    );
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::error!("session_log: failed to spawn detached record child");
        return None;
    };
    if let Some(mut stdin) = child.stdin.take()
        // Small, already-truncated payload: this write fits the pipe buffer
        // and completes without the child having read anything yet.
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        tracing::error!("session_log: failed to pipe event to detached child: {e}");
    }
    // Not waited on by the caller: the child is process-group-detached and
    // outlives us.
    Some(child)
}

/// The `llmenv session-log-record` child, before its stderr log and process group are set.
pub(crate) fn record_command(
    exe: std::path::PathBuf,
    checkpoint: Option<&std::path::Path>,
) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg("session-log-record")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    if let Some(path) = checkpoint {
        cmd.arg("--checkpoint").arg(path);
    }
    cmd
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
pub(crate) fn run_record(
    payload_json: &str,
    checkpoint: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // The error is logged here and not in `run_record_with`: a test of that function would
    // otherwise reach this callsite outside a subscriber and break the capture test below.
    run_record_with(payload_json, checkpoint, run_record_inner).inspect_err(|e| {
        tracing::error!("session_log: detached record failed: {e}");
    })
}

/// [`run_record`] with the record call injected, so a test needs no memory backend. A payload that
/// does not parse cannot succeed on a retry, so its checkpoint is deleted with a successful one's.
fn run_record_with(
    payload_json: &str,
    checkpoint: Option<&std::path::Path>,
    record: impl FnOnce(RecordPayload) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // The parent can fail to pipe all of the payload, so the checkpoint backs it up.
    let parsed = checkpoint::inputs_or_checkpoint::<RecordPayload>(payload_json, checkpoint);
    let unparseable = parsed.is_err();
    let result = parsed.and_then(record);
    match &result {
        Ok(()) => checkpoint::finish(checkpoint),
        Err(_) => {
            if unparseable {
                checkpoint::finish(checkpoint);
            }
        }
    }
    result
}

fn run_record_inner(payload: RecordPayload) -> anyhow::Result<()> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::memory::memory_url(&config, config_dir, &active)?.into_url()?;
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
#[allow(clippy::unwrap_used, clippy::panic)]
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
    fn spawn_record_writes_a_checkpoint_and_passes_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let child = spawn_record_in(Some(dir.path()), "sess-1", &ev());
        let listed = checkpoint::list(dir.path());
        let [checkpoint::Entry::Ready(path, cp)] = &listed[..] else {
            panic!("expected one checkpoint: {listed:?}");
        };
        assert_eq!(cp.kind, JobKind::SessionLogRecord);
        assert_eq!(cp.inputs["session_id"], "sess-1");
        assert!(!cp.inputs["request_id"].as_str().unwrap().is_empty());
        let cmd = record_command("llmenv".into(), Some(path));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[..2], ["session-log-record", "--checkpoint"]);
        if let Some(mut child) = child {
            reap(&mut child, std::time::Duration::from_secs(5));
        }
    }

    #[test]
    fn a_failed_record_keeps_its_checkpoint_and_a_successful_one_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let cp =
            checkpoint::Checkpoint::new(JobKind::SessionLogRecord, serde_json::json!({}), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        let json = serde_json::to_string(&payload_with_id("abc")).unwrap();
        run_record_with(&json, Some(&file), |_| anyhow::bail!("down")).unwrap_err();
        assert!(file.exists());
        run_record_with(&json, Some(&file), |_| Ok(())).unwrap();
        assert!(!file.exists());
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        run_record_with("not json", Some(&file), |_| Ok(())).unwrap_err();
        assert!(!file.exists(), "an unparseable payload cannot be retried");
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
            || run_record("not json", None).unwrap_err(),
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

    proptest::proptest! {
        #[test]
        fn record_payload_survives_serialization(
            session in ".{0,30}", id in "[0-9a-f]{0,16}", content in ".{0,60}",
        ) {
            let mut event = ev();
            event.content = content;
            let payload = RecordPayload { session_id: session, event, request_id: id };
            let back: RecordPayload =
                serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
            proptest::prop_assert_eq!(back.session_id, payload.session_id);
            proptest::prop_assert_eq!(back.request_id, payload.request_id);
            proptest::prop_assert_eq!(back.event, payload.event);
        }
    }

    #[test]
    fn a_payload_from_an_older_llmenv_parses_with_an_empty_request_id() {
        let json = serde_json::json!({"session_id": "s", "event": ev()}).to_string();
        let back: RecordPayload = serde_json::from_str(&json).unwrap();
        assert!(back.request_id.is_empty());
    }

    #[test]
    fn record_request_id_follows_session_time_kind_and_content() {
        let base = record_request_id("s1", &ev(), "n");
        assert_eq!(base, record_request_id("s1", &ev(), "n"));
        assert_ne!(base, record_request_id("s2", &ev(), "n"));
        let mut later = ev();
        later.ts = "t2".into();
        assert_ne!(base, record_request_id("s1", &later, "n"));
        let mut other = ev();
        other.content = "bye".into();
        assert_ne!(base, record_request_id("s1", &other, "n"));
        assert_ne!(
            base,
            record_request_id("s1", &ev(), "m"),
            "two identical events in one second are two records"
        );
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
