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

/// Tell the agent that one session log record was not written, and why. The hook's stderr never
/// reaches the agent, so a log line alone is lost (#3398).
fn record_lost(cause: &str) {
    crate::agent_notice::record_logged(format!(
        "llmenv: a session log record was not written: {cause}. Fix: run `llmenv doctor` to \
         check session_log.file.path."
    ));
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
        record_lost("llmenv cannot find its own program");
        return None;
    };
    let payload = RecordPayload {
        session_id: session_id.to_string(),
        event: ev.clone(),
        request_id: record_request_id(session_id, ev, &checkpoint::run_tag()),
    };
    let Ok(payload_json) = serde_json::to_string(&payload) else {
        record_lost("the event cannot be serialized");
        return None;
    };
    let checkpoint = checkpoint::begin(
        state_dir,
        JobKind::SessionLogRecord,
        &payload,
        Some(session_id),
    );
    let mut cmd = record_command(exe, checkpoint.as_deref());
    crate::hook_run::redirect_stderr_to_detached_log(
        &mut cmd,
        crate::hook_run::detached_child_log_path(),
    );
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = crate::trace::spawn_counted("session_log_record", &mut cmd) else {
        record_lost("the record job cannot start");
        return None;
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        record_lost(&format!("the event cannot be sent to the record job: {e}"));
        // The child would read no event and record a second notice of its own, so it is stopped.
        if let Err(kill) = child.kill() {
            tracing::error!(error = %kill, "cannot stop the record job that got no event");
        }
        return None;
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
/// Emits a `[LLMENV_DETACHED] session-log-record` trace line with wall time,
/// CPU time, and exit status when `LLMENV_TRACE_TIMING` is set (#3014).
///
/// # Errors
/// Malformed payload, no active memory backend, an invalid backend URL, or
/// the MCP call itself failing.
pub(crate) fn run_record(
    payload_json: &str,
    checkpoint: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    // The error is logged here and not in `run_record_with`: a test of that function would
    // otherwise reach this callsite outside a subscriber and break the capture test below.
    let result = run_record_with(payload_json, checkpoint, run_record_inner);
    crate::trace::emit_detached_trace(&crate::trace::DetachedJob {
        kind: "session-log-record",
        request_id: crate::trace::json_str(payload_json, "request_id").as_deref(),
        started: t0,
        exit: i32::from(result.is_err()),
    });
    // The notice names the setting to check. The cause stays in the log (#3507).
    result.inspect_err(|e| {
        crate::agent_notice::record_job_notice_with_cause(
            "llmenv: a session log record was not written. Fix: run `llmenv doctor` to check \
             session_log.file.path. The cause is in the llmenv log.",
            e,
        );
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
    // A checkpoint that could not be read may succeed on a retry, so its file stays (#2779).
    let unparseable = parsed.as_ref().err().is_some_and(|e| !e.keep_checkpoint);
    let result = parsed
        .map_err(checkpoint::InputsError::into_error)
        .and_then(record);
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
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?.into_url()?;
    let client = McpHttpClient::new(url, RECORD_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let state_dir = crate::paths::state_dir()
        .inspect_err(|e| {
            crate::agent_notice::record_logged(format!(
                "llmenv: the session log record job has no state folder, so it skips the \
                 duplicate check and an event can be recorded twice: {e:#}. Fix: set HOME, or set \
                 LLMENV_STATE_DIR to a valid absolute path."
            ));
        })
        .ok();
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
mod tests;
