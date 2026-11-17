//! Detached job trace emitter and CPU-time probe (#3014).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::trace::emit_trace_line;

/// Detached jobs this process has started. The hook marker reports it as `spawned_jobs`.
static SPAWNED_JOBS: AtomicU64 = AtomicU64::new(0);

/// Start a detached job, count it when the start succeeds, and print one `[LLMENV_SPAWN]` line
/// with `exit=detached` (or `spawn-error` when the start fails). `label` is a fixed tag. The
/// duration is the time the start took, not the job's own run time (#3013).
pub(crate) fn spawn_counted(
    label: &str,
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    let started = std::time::Instant::now();
    let result = cmd.spawn();
    let exit = if result.is_ok() {
        SPAWNED_JOBS.fetch_add(1, Ordering::Relaxed);
        crate::trace::SpawnExit::Detached
    } else {
        crate::trace::SpawnExit::Error
    };
    crate::trace::emit_spawn_trace(label, started.elapsed(), exit);
    result
}

/// How many detached jobs this process has started.
pub(crate) fn spawned_jobs() -> u64 {
    SPAWNED_JOBS.load(Ordering::Relaxed)
}

/// One detached job that is about to exit.
pub(crate) struct DetachedJob<'a> {
    /// Fixed tag such as `consolidation` or `icm-store`.
    pub kind: &'a str,
    /// The request id the parent logged for this job, when the job has one.
    pub request_id: Option<&'a str>,
    /// When the job started.
    pub started: std::time::Instant,
    /// The job exit code: 0 for success, 1 for a failure.
    pub exit: i32,
}

/// Emit `[LLMENV_DETACHED] <kind> <wall_us>us cpu_us=<n> mcp_calls=<n> exit=<code>[ request_id=<id>]`.
///
/// The line goes to the trace sink. A detached job has no visible stderr, so without
/// `LLMENV_TRACE_FILE` the line is also appended to `detached-trace.log` in the state dir.
pub(crate) fn emit_detached_trace(job: &DetachedJob<'_>) {
    if !crate::trace::enabled() {
        return;
    }
    let stats = crate::trace::lines::DetachedStats {
        wall_us: u64::try_from(job.started.elapsed().as_micros()).unwrap_or(u64::MAX),
        cpu_us: cpu_time_us(),
        mcp_calls: crate::mcp_trace::mcp_call_count(),
        exit: job.exit,
    };
    let line = crate::trace::lines::detached_line(job.kind, stats, job.request_id);
    emit_trace_line(&line);
    if crate::trace::file_sink_path().is_none() {
        match llmenv_paths::state_dir() {
            Ok(dir) => append_to_state_log(&dir.join(STATE_LOG), &line),
            Err(e) => crate::agent_notice::record_logged(format!(
                "llmenv: a detached job trace line was dropped, because the state folder cannot \
                 be found: {e:#}. Fix: run `llmenv doctor` to check the state folder."
            )),
        }
    }
}

/// The detached job log in the state dir, used when `LLMENV_TRACE_FILE` is unset.
const STATE_LOG: &str = "detached-trace.log";

/// The size at which the state log moves to `<name>.1`. One older copy is kept, so the pair stays
/// near twice this size. A detached job runs after every session, so the log cannot grow without a
/// bound.
const STATE_LOG_MAX_BYTES: u64 = 1024 * 1024;

fn append_to_state_log(path: &std::path::Path, line: &str) {
    if is_oversized_regular_file(path) {
        rotate_state_log(path);
    }
    crate::trace::append_trace_line(path, line);
}

/// True when `path` is a regular file over the size limit. `symlink_metadata` does not follow a
/// link, so a link is never rotated, and a link cannot move the log of its target (#3139).
fn is_oversized_regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() > STATE_LOG_MAX_BYTES)
}

/// Rename `path` to `<name>.1` relative to its open folder. The rename resolves no path component
/// again, so a folder swapped for a link after the check cannot move the rename elsewhere (#3139).
fn rotate_state_log(path: &std::path::Path) {
    let dir_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    let Some(name) = path.file_name() else {
        return;
    };
    let mut older = name.to_owned();
    older.push(".1");
    let rotated = crate::paths::dirfd::open_dir_nofollow(dir_path).and_then(|dir| {
        rustix::fs::renameat(&dir, name, &dir, older.as_os_str()).map_err(std::io::Error::from)
    });
    if let Err(e) = rotated {
        crate::agent_notice::record_logged(format!(
            "llmenv: cannot rotate the detached job trace log {}: {e}. Fix: check that the state \
             folder is writable.",
            path.display()
        ));
    }
}

/// Return user+sys CPU time of this process as microseconds.
fn cpu_time_us() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    let secs = u64::try_from(t.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(t.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

/// Read the string field `key` from a JSON payload, or `None` when the payload or field is absent.
pub(crate) fn json_str(payload_json: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = match serde_json::from_str(payload_json) {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!("detached trace: payload is not JSON, so it has no {key}: {e}");
            return None;
        }
    };
    value.get(key)?.as_str().map(str::to_string)
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "a test: a failed setup must stop it")]
mod tests {
    use super::*;

    fn oversized(path: &std::path::Path) {
        let big = vec![b'x'; usize::try_from(STATE_LOG_MAX_BYTES).expect("limit fits") + 1];
        std::fs::write(path, big).expect("write an oversized log");
    }

    #[test]
    fn an_oversized_regular_log_moves_to_the_older_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = dir.path().join(STATE_LOG);
        oversized(&log);
        append_to_state_log(&log, "line");
        assert!(dir.path().join(format!("{STATE_LOG}.1")).exists());
    }

    #[test]
    fn a_link_to_an_oversized_log_is_never_rotated() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target.log");
        oversized(&target);
        let log = dir.path().join(STATE_LOG);
        std::os::unix::fs::symlink(&target, &log).expect("plant a link");
        assert!(
            !is_oversized_regular_file(&log),
            "a link is not a regular log"
        );
        append_to_state_log(&log, "line");
        assert!(target.exists(), "the link target must stay in place");
        assert!(!dir.path().join(format!("{STATE_LOG}.1")).exists());
    }
}
