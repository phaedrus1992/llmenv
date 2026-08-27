//! Detached-process stderr redirection, shared by `hook_run`'s subprocess
//! spawns and `session_log`'s own detached record/store paths.

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
/// `dir_mode` is forwarded to `open_bounded_log`, which does the 0700
/// hardening itself (#1196) — pass `LogDirMode::Inherit` when `log_path`'s
/// directory may be shared with a process running under a different uid
/// (e.g. a user-configured `index_path`). `context` names the caller in the
/// debug-level "log unavailable" message, since that message is shared across
/// callers with different failure consequences (#1141). No `max_bytes`
/// parameter: every caller bounds to the same [`BOUNDED_LOG_MAX_BYTES`] now
/// that the two call sites' previously-distinct constants turned out to be
/// identical (#1141) — a parameter every caller passes the same value for
/// isn't a real degree of freedom.
pub(crate) fn redirect_stderr_to_bounded_log(
    cmd: &mut std::process::Command,
    log_path: &std::path::Path,
    dir_mode: llmenv_mcp::proxy::LogDirMode,
    context: &str,
) {
    cmd.stderr(std::process::Stdio::null());
    match llmenv_mcp::proxy::open_bounded_log(log_path, BOUNDED_LOG_MAX_BYTES, dir_mode) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            tracing::debug!("{context}: log unavailable ({e:#}), stderr discarded");
        }
    }
}

/// Send a detached child's stderr to the shared bounded log instead of
/// discarding it (#1133, the same remedy as #1091).
///
/// `Stdio::null()` leaves such a child with no report channel whatsoever: its
/// own `tracing` events go to a fmt layer writing to that same null stderr, so
/// a failure is discarded twice over.
///
/// `log_path` resolves the log's location; every real caller passes
/// [`detached_child_log_path`]. Parameterized rather than calling it
/// directly so a test can inject a fixed tempdir path — this workspace
/// forbids `unsafe`, so a test can't safely override `state_dir()`'s
/// `LLMENV_STATE_DIR`/`HOME` env vars to control the real resolver instead.
pub(crate) fn redirect_stderr_to_detached_log(
    cmd: &mut std::process::Command,
    log_path: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) {
    match log_path() {
        // Always state_dir-rooted, so `LogDirMode::OwnerOnly` is safe.
        Ok(path) => redirect_stderr_to_bounded_log(
            cmd,
            &path,
            llmenv_mcp::proxy::LogDirMode::OwnerOnly,
            "detached child",
        ),
        Err(e) => {
            cmd.stderr(std::process::Stdio::null());
            tracing::debug!("detached child: cannot resolve log path ({e:#}), stderr discarded");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // #1133: the detached memory children were spawned with
    // `stderr(Stdio::null())`, so nothing they reported could reach anyone —
    // including the `tracing` events meant to compensate, whose sink is that
    // same discarded stderr.
    #[test]
    fn redirect_stderr_to_bounded_log_captures_child_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2");
        redirect_stderr_to_bounded_log(
            &mut cmd,
            &log,
            llmenv_mcp::proxy::LogDirMode::OwnerOnly,
            "test",
        );

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log)
            .expect("a detached child's stderr must reach a file, not /dev/null");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }

    // Pins the shared log name: the three detached children, the docs, and any
    // operator told where to look must all agree on one path.
    #[test]
    fn detached_child_log_path_is_named_under_the_state_dir() {
        let path = detached_child_log_path().expect("test");
        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("detached-hook.log")
        );
        assert!(path.starts_with(crate::paths::state_dir().expect("test")));
    }

    /// End-to-end coverage for `redirect_stderr_to_detached_log` itself, not
    /// just the `redirect_stderr_to_bounded_log` helper it delegates to
    /// (`redirect_stderr_to_bounded_log_captures_child_stderr` above already
    /// covers that) — this calls the exact same function signature real
    /// callers do, with an injected path resolver instead of the real
    /// `detached_child_log_path`, so it's the one test that would catch this
    /// function's own body being replaced wholesale.
    #[test]
    fn redirect_stderr_to_detached_log_writes_to_the_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2");
        redirect_stderr_to_detached_log(&mut cmd, || Ok(log_path.clone()));

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log_path)
            .expect("a detached child's stderr must reach the resolved log path");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }
}
