//! The proxy's files in its state directory: the pidfile, its temp file, and the stderr log.
//!
//! A failure on any of these is recorded for the agent. The engine drops a hook's stderr, so a
//! log line alone would reach no one.

use std::path::Path;
use std::process::Stdio;

use super::{PidfileError, is_alive, log_path_for, open_proxy_log, read_pidfile};

/// Delete a stale pidfile. A failed delete is recorded with its path, not dropped.
fn clear_pidfile(pid_path: &Path, reason: &str) {
    match std::fs::remove_file(pid_path) {
        Ok(()) => tracing::debug!("cleared proxy pidfile {}: {reason}", pid_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => llmenv_util::agent_notice::record_logged(format!(
            "llmenv: cannot remove proxy pidfile {} ({e}; {reason}). Fix: remove the file by hand, \
             or fix the permissions of its folder.",
            pid_path.display()
        )),
    }
}

/// Clears a pidfile that names a process which is not running.
///
/// Called once the port has proved a proxy is up. A pidfile naming a dead or
/// recycled pid is worse than no pidfile — the old fast path read any non-empty
/// pidfile as proof of life, so a wrong pid could never be recovered from
/// (#1085). A pid that *is* alive is left alone: `kill -0` cannot prove which
/// process owns the port, so the running pid is the best available answer to
/// "who do I signal".
///
/// Best-effort and non-fatal: liveness has already been established by the port,
/// so failing an export over an unwritable pidfile would trade a cosmetic
/// problem for a real one.
pub(super) fn reconcile_pidfile(pid_path: &Path) {
    match read_pidfile(pid_path) {
        Ok(None) => {}
        // Only a definite "not running" justifies deleting the record. `None`
        // means the check itself failed, and deleting on a guess would discard a
        // valid pid.
        Ok(Some(pid)) => {
            if is_alive(pid) == Some(false) {
                clear_pidfile(pid_path, &format!("pid {pid} is not running"));
            }
        }
        Err(PidfileError::Unparseable(e)) => {
            clear_pidfile(pid_path, &format!("unparseable: {e}"));
        }
        // An I/O error is not evidence the contents are wrong, and deleting would
        // fail for the same reason. Say so instead, for the agent.
        Err(PidfileError::Io(e)) => llmenv_util::agent_notice::record_logged(format!(
            "llmenv: cannot read proxy pidfile {} ({e}). Fix: fix its permissions, or remove the file.",
            pid_path.display()
        )),
    }
}

/// Removes the pidfile temp after a failed write or publish. A failed removal is recorded, so a
/// leftover temp file is not silent (#2657).
pub(super) fn discard_pidfile_temp(tmp: &Path) {
    if let Err(e) = std::fs::remove_file(tmp) {
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: could not remove pidfile temp {} ({e}). Fix: remove the file by hand.",
            tmp.display()
        ));
    }
}

/// The stderr of a new proxy. A log that cannot be opened gives a null stream. The proxy still
/// starts, because a missing diagnostic is a smaller problem than no memory backend (#1086).
/// The lost log is recorded, so the agent can see why a startup failure has no detail.
pub(super) fn proxy_stderr(pid_path: &Path) -> Stdio {
    let log_path = log_path_for(pid_path);
    match open_proxy_log(&log_path) {
        Ok(file) => Stdio::from(file),
        Err(e) => {
            llmenv_util::agent_notice::record_logged(format!(
                "llmenv: cannot open the proxy stderr log {} ({e:#}), so mcp-proxy starts with its \
                 stderr discarded. Fix: make the log path writable, then start a new session.",
                log_path.display()
            ));
            Stdio::null()
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod notice_tests {
    use super::*;

    /// A pidfile that cannot be removed is named for the agent. A directory at the path makes
    /// `remove_file` fail for a real reason (#3217).
    #[test]
    fn failed_pidfile_removal_is_recorded_for_the_agent() {
        let dir = tempfile::tempdir().expect("test");
        let pid_path = dir.path().join("pidfile-3217-stuck.pid");
        std::fs::create_dir(&pid_path).expect("test");
        clear_pidfile(&pid_path, "pid 1 is not running");
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("pidfile-3217-stuck"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A pidfile that cannot be read is named for the agent. A directory at the path makes the
    /// read fail with an I/O error (#3217).
    #[test]
    fn unreadable_pidfile_is_recorded_for_the_agent() {
        let dir = tempfile::tempdir().expect("test");
        let pid_path = dir.path().join("pidfile-3217-unreadable.pid");
        std::fs::create_dir(&pid_path).expect("test");
        reconcile_pidfile(&pid_path);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("pidfile-3217-unreadable"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A pidfile temp that cannot be removed is named for the agent (#3217).
    #[test]
    fn failed_pidfile_temp_removal_is_recorded_for_the_agent() {
        let dir = tempfile::tempdir().expect("test");
        let tmp = dir.path().join("pidfile-3217-temp.tmp");
        std::fs::create_dir(&tmp).expect("test");
        discard_pidfile_temp(&tmp);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("pidfile-3217-temp"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A proxy whose stderr log cannot be opened names the log for the agent. A directory at the
    /// log path makes the open fail (#3217).
    #[test]
    fn unopenable_proxy_log_is_recorded_for_the_agent() {
        let dir = tempfile::Builder::new()
            .prefix("stderr-3217-")
            .tempdir()
            .expect("test");
        let pid_path = dir.path().join("mcp-proxy.pid");
        let log = log_path_for(&pid_path);
        std::fs::create_dir(&log).expect("test");
        drop(proxy_stderr(&pid_path));
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains(&log.display().to_string()), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }
}
