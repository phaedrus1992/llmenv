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
    let cp = checkpoint::Checkpoint::new(JobKind::SessionLogRecord, serde_json::json!({}), None);
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
fn run_record_rejects_malformed_payload_json_with_a_notice() {
    let err = run_record("not json", None).unwrap_err();

    assert!(err.to_string().to_lowercase().contains("expected"));
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("a session log record was not written") && notices.contains("Fix:"),
        "the failure must reach the agent as a notice: {notices}"
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
