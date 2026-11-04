//! Stderr of llmenv's detached and bounded children, pointed at a size-bounded log.

/// Rotation bound shared by every size-bounded stderr log this module opens —
/// the indexer's diagnostic log and the detached hook children's shared log
/// (#1086/#1091 share the "size-bounded" shape and, previously, a
/// byte-for-byte identical constant under two names; merged under #1141).
/// Smaller than the mcp-proxy log: these children run often but write
/// nothing unless they fail, and indexing runs are far less frequent than
/// proxy restarts.
const BOUNDED_LOG_MAX_BYTES: u64 = 1 << 19; // 512 KiB

/// Path of the stderr log shared by llmenv's detached hook children —
/// `<state_dir>/detached-hook.log`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub(crate) fn detached_child_log_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::state_dir()?.join("detached-hook.log"))
}

/// Point `cmd`'s stderr at `log_path` as a size-bounded diagnostic log.
///
/// Sets the null baseline first, unconditionally (#1139): `Command`'s default
/// for an unset stdio is `Stdio::inherit()`, not discarded, so a caller that
/// only overrode stderr on the `Ok` branch would leave the child holding
/// whichever fd this process's own stderr happens to be on a log-open
/// failure — the exact hang/leak this redirect exists to prevent. If the log
/// can't be opened the child still runs with stderr discarded — a missing
/// diagnostic is a smaller problem than skipping the work.
///
/// `harden_dir` is forwarded to `open_bounded_log`, which does the 0700
/// hardening itself (#1196) — pass `false` when `log_path`'s directory may be
/// shared with a process running under a different uid (e.g. a
/// user-configured `index_path`). `context` names the caller in the
/// debug-level "log unavailable" message, since that message is shared across
/// callers with different failure consequences (#1141). No `max_bytes`
/// parameter: every caller bounds to the same [`BOUNDED_LOG_MAX_BYTES`] now
/// that the two call sites' previously-distinct constants turned out to be
/// identical (#1141) — a parameter every caller passes the same value for
/// isn't a real degree of freedom.
pub(super) fn redirect_stderr_to_bounded_log(
    cmd: &mut std::process::Command,
    log_path: &std::path::Path,
    harden_dir: bool,
    context: &str,
) {
    cmd.stderr(std::process::Stdio::null());
    match crate::mcp::proxy::open_bounded_log(log_path, BOUNDED_LOG_MAX_BYTES, harden_dir) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "{context}: log unavailable ({e:#}), so the child's stderr is discarded. Fix: make \
                 the log path a regular file that you own."
            ));
        }
    }
}

/// Send a detached child's stderr to the shared bounded log instead of discarding it (#1133, the
/// same remedy as #1091). `log` is the resolved [`detached_child_log_path`].
///
/// `Stdio::null()` leaves such a child with no report channel whatsoever: its own `tracing`
/// events go to a fmt layer writing to that same null stderr, so a failure is discarded twice over.
pub(crate) fn redirect_stderr_to_detached_log(
    cmd: &mut std::process::Command,
    log: anyhow::Result<std::path::PathBuf>,
) {
    match log {
        // Always state_dir-rooted, so `harden_dir: true` is safe.
        Ok(path) => redirect_stderr_to_bounded_log(cmd, &path, true, "detached child"),
        Err(e) => {
            cmd.stderr(std::process::Stdio::null());
            crate::agent_notice::record_logged(format!(
                "detached child: cannot resolve log path ({e:#}), so the child's stderr is \
                 discarded. Fix: set HOME, so the state folder can be found."
            ));
        }
    }
}
