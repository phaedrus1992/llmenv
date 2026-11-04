//! Stderr of llmenv's detached and bounded children, pointed at a size-bounded log.

/// Size limit of the shared child logs. Children run often but write only when they fail, so
/// the limit stays small.
const BOUNDED_LOG_MAX_BYTES: u64 = 1 << 19; // 512 KiB

/// Path of the stderr log shared by llmenv's detached hook children,
/// `<state_dir>/detached-hook.log`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub(crate) fn detached_child_log_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::state_dir()?.join("detached-hook.log"))
}

/// Point `cmd`'s stderr at `log_path` as a size-bounded log.
///
/// The null baseline is set first. A `Command` inherits stderr by default, so a log that fails to
/// open would otherwise leave the child on this process's stderr, which the engine may drop.
///
/// `harden_dir` is forwarded to `open_bounded_log`. Pass `false` when the log folder may be
/// shared with a process under a different uid. `context` names the caller in the notice.
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
                "{context}: log unavailable ({e:#}), so the child's stderr is discarded. Fix: \
                 check the log path and the folder that holds it."
            ));
        }
    }
}

/// Send a detached child's stderr to the shared bounded log. `log` is the result of
/// [`detached_child_log_path`].
///
/// A child with `Stdio::null()` has no report channel, so its `tracing` events are lost too.
pub(crate) fn redirect_stderr_to_detached_log(
    cmd: &mut std::process::Command,
    log: anyhow::Result<std::path::PathBuf>,
) {
    match log {
        // The path is under the state folder, so `harden_dir: true` is safe.
        Ok(path) => redirect_stderr_to_bounded_log(cmd, &path, true, "detached child"),
        Err(e) => {
            cmd.stderr(std::process::Stdio::null());
            crate::agent_notice::record_logged(format!(
                "detached child: cannot resolve log path ({e:#}), so the child's stderr is \
                 discarded. Fix: set HOME, or set LLMENV_STATE_DIR to a valid absolute path."
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BOUNDED_LOG_MAX_BYTES;

    #[test]
    fn the_child_log_limit_is_512_kib() {
        assert_eq!(BOUNDED_LOG_MAX_BYTES, 512 * 1024);
    }
}
