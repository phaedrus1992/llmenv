//! Routes one session-log event to the configured sinks. Fail-soft: a sink
//! failure logs and never fails the hook.

use tracing::warn;

use llmenv_config::LogLevel;

use crate::config::SessionLog;
use crate::session_log::event::{EventScope, SessionLogEvent};

/// Append `ev` to the configured sinks: the JSONL file (if enabled and
/// `ev.log_level() <= file.level`, written synchronously) and, for
/// agent-session-scoped events, the ICM transcript (if enabled and
/// `ev.log_level() <= transcript.level` — dispatched via a detached child, see
/// `session_log::detached`, so this never blocks on the network). Fail-soft.
pub(super) fn emit_session_log(ev: SessionLogEvent, cfg: &SessionLog, session_id: Option<&str>) {
    let max = cfg.max_content_bytes.unwrap_or(16_384);
    let ev = ev.truncated(max);
    let level = ev.log_level();
    if cfg.file_wants(level) {
        let path = resolve_file_sink_path(cfg, crate::session_log::default_file_path);
        if let Some(p) = path {
            crate::session_log::FileSink::new(p).append(&ev.to_jsonl());
        }
    }
    if let Some(sid) = session_id.filter(|_| wants_transcript(cfg, ev.scope, level)) {
        // Detached: process-group-detached and outlives us regardless.
        let _detached_child = crate::session_log::detached::spawn_record(sid, &ev);
    }
}

/// Whether the transcript sink takes an event: only an agent-session event, at a level the sink
/// keeps. A process-level event has no transcript session to attach to.
fn wants_transcript(cfg: &SessionLog, scope: EventScope, level: LogLevel) -> bool {
    cfg.transcript_wants(level) && scope == EventScope::AgentSession
}

/// The file-sink path: the configured path with `~` expanded, else `default`. A failure logs
/// at `warn` with the effect and the fix, so a missing file is not silent.
fn resolve_file_sink_path(
    cfg: &SessionLog,
    default: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    crate::session_log::file_sink::resolve_file_path_with(cfg.file_path(), default)
        .inspect_err(|e| {
            warn!(
                "session_log: file sink disabled, no path resolved, so events are not written to a file: {e:#}. Fix: set HOME, or set session_log.file.path to an absolute path."
            );
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use llmenv_config::{FileSinkConfig, LogLevel};

    fn transcript_cfg(enabled: bool) -> SessionLog {
        SessionLog {
            file: None,
            transcript: Some(llmenv_config::TranscriptSinkConfig {
                enabled,
                level: LogLevel::Info,
                retention_days: None,
            }),
            max_content_bytes: None,
        }
    }

    #[test]
    fn an_agent_session_event_at_a_kept_level_goes_to_the_transcript() {
        assert!(wants_transcript(
            &transcript_cfg(true),
            EventScope::AgentSession,
            LogLevel::Info
        ));
    }

    #[test]
    fn a_process_event_never_goes_to_the_transcript() {
        assert!(!wants_transcript(
            &transcript_cfg(true),
            EventScope::Process,
            LogLevel::Info
        ));
    }

    #[test]
    fn a_disabled_transcript_sink_takes_no_event() {
        assert!(!wants_transcript(
            &transcript_cfg(false),
            EventScope::AgentSession,
            LogLevel::Info
        ));
    }

    #[test]
    fn file_sink_without_path_warns_with_fix_when_default_fails() {
        let cfg = SessionLog::default();
        let mut resolved = None;
        let logs = crate::test_log_capture::capture_logs(|| {
            resolved = resolve_file_sink_path(&cfg, || Err(anyhow::anyhow!("HOME is unset")));
        });
        assert!(resolved.is_none(), "no path must resolve");
        assert!(logs.contains("WARN"), "expected a warning, got: {logs}");
        assert!(logs.contains("HOME is unset"), "cause missing: {logs}");
        assert!(logs.contains("Fix: set HOME"), "fix missing: {logs}");
    }

    #[test]
    fn file_sink_configured_path_wins_without_consulting_default() {
        let cfg = SessionLog {
            file: Some(FileSinkConfig {
                enabled: true,
                level: LogLevel::Info,
                path: Some("/configured/session.jsonl".to_owned()),
            }),
            transcript: None,
            max_content_bytes: None,
        };
        let mut consulted = false;
        let resolved = resolve_file_sink_path(&cfg, || {
            consulted = true;
            Err(anyhow::anyhow!("default must not be consulted"))
        });
        assert!(!consulted, "a configured path must skip the default lookup");
        assert_eq!(
            resolved,
            Some(std::path::PathBuf::from("/configured/session.jsonl"))
        );
    }
}
