//! Stopping the probe process and its process group.

/// Send `SIGTERM` to the process group that `pid` leads. The probe starts the server with
/// `process_group(0)`, so `pid` is the group's own id. A direct syscall: no `kill` process starts.
pub(super) fn signal_group(pid: u32) -> std::io::Result<()> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        // A group of pid 1 would reach every process that this user may signal. A child that was
        // just spawned cannot have pid 1, so this is a guard against a wrong id.
        .ok_or_else(|| std::io::Error::other(format!("pid {pid} is not a process")))?;
    if pid.as_raw_nonzero().get() == 1 {
        return Err(std::io::Error::other(
            "refusing to signal the process group of pid 1",
        ));
    }
    rustix::process::kill_process_group(pid, rustix::process::Signal::TERM)
        .map_err(std::io::Error::from)
}

/// Stop the child and its process group. `kill_on_drop` is only the backstop for an early return.
pub(super) async fn stop_group(child: &mut tokio::process::Child, command: &str) {
    if let Some(pid) = child.id()
        && let Err(e) = signal_group(pid)
    {
        // The server may still run, and hook stderr never reaches the agent (#3204).
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: cannot stop the process group of MCP server `{command}`: {e}. What the \
             server started may still run. Fix: stop that process by hand."
        ));
    }
    if let Err(e) = child.kill().await {
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: cannot stop the MCP text probe `{command}`: {e}. Fix: stop that process by \
             hand."
        ));
    }
}
