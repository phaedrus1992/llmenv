//! Detached job trace emitter and CPU-time probe (#3014).

use crate::trace::emit_trace_line;

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
            Err(e) => tracing::warn!("detached trace: no state dir, line dropped: {e:#}"),
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
        tracing::warn!("detached trace: cannot rotate {}: {e}", path.display());
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // The state log appends below the cap, and moves to `.1` above it, so it stays bounded.
    #[test]
    fn the_state_log_rotates_once_it_passes_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_LOG);
        append_to_state_log(&path, "first");
        append_to_state_log(&path, "second");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        let big = vec![b'x'; usize::try_from(STATE_LOG_MAX_BYTES).unwrap() + 1];
        std::fs::write(&path, big).unwrap();
        append_to_state_log(&path, "after rotation");
        let older = dir.path().join(format!("{STATE_LOG}.1"));
        assert!(older.exists(), "the full log moves to .1");
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert_eq!(fresh.lines().count(), 1);
        assert!(fresh.contains("after rotation"));
    }

    // CPU time of this process is a number that never goes backwards.
    #[test]
    fn cpu_time_does_not_go_backwards() {
        let a = cpu_time_us();
        let mut x = 0u64;
        for i in 0..200_000u64 {
            x = x.wrapping_add(i.wrapping_mul(31));
        }
        assert!(x != 1);
        assert!(cpu_time_us() >= a);
    }

    // A missing field, a non-string field, and a bad payload each give `None`.
    #[test]
    fn json_str_gives_none_for_a_missing_or_bad_field() {
        assert_eq!(json_str(r#"{"a":"b"}"#, "a").as_deref(), Some("b"));
        assert_eq!(json_str(r#"{"a":"b"}"#, "c"), None);
        assert_eq!(json_str(r#"{"a":1}"#, "a"), None);
        assert_eq!(json_str("{oops", "a"), None);
    }

    #[cfg(unix)]
    fn oversized(path: &std::path::Path) {
        let big = vec![b'x'; usize::try_from(STATE_LOG_MAX_BYTES).expect("limit fits") + 1];
        std::fs::write(path, big).expect("write an oversized log");
    }

    #[cfg(unix)]
    #[test]
    fn an_oversized_regular_log_moves_to_the_older_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = dir.path().join(STATE_LOG);
        oversized(&log);
        append_to_state_log(&log, "line");
        assert!(dir.path().join(format!("{STATE_LOG}.1")).exists());
    }

    #[cfg(unix)]
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
