//! Whether a pid names a process that this user can signal.
//!
//! The proxy stop path asks this question before it sends a signal.

/// Whether `pid` is a running process that this user can signal.
pub(crate) fn is_running(pid: rustix::process::Pid) -> anyhow::Result<bool> {
    match rustix::process::test_kill_process(pid) {
        Ok(()) => Ok(true),
        // EPERM is a process of another user. It is not this user's to stop, so it is not
        // running here (#3810).
        Err(rustix::io::Errno::SRCH | rustix::io::Errno::PERM) => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "cannot check pid {}: {e}",
            pid.as_raw_nonzero()
        )),
    }
}
