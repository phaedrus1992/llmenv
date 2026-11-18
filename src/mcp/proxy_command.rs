//! Inspect the command line of a process, to confirm that it is the memory proxy (#2923, #2948).

/// A command line that starts the memory proxy: `mcp-proxy … -- <icm> serve`, where `<icm>` is
/// `icm` or an absolute path to it (#2948). A script runs through its interpreter, and `uvx` runs
/// the tool by name, so `mcp-proxy` is the first or the second word.
pub(crate) fn is_proxy_command(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let names_proxy = words
        .iter()
        .take(2)
        .any(|w| w.rsplit('/').next() == Some("mcp-proxy"));
    let serves_icm = matches!(
        words.as_slice(),
        [.., "--", icm, "serve"] if icm.rsplit('/').next() == Some("icm")
    );
    names_proxy && serves_icm
}

/// The command line of `pid`, or `None` when the process is gone. `-ww` stops `ps` from cutting
/// a long line, which would hide the `-- icm serve` tail.
pub(crate) fn command_of(pid: u32) -> anyhow::Result<Option<String>> {
    // An absolute path: a `ps` earlier on PATH could answer for any pid.
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = crate::process::run_traced("proxy:ps", cmd, PS_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("cannot run /bin/ps to read the command of pid {pid}: {e}"))?;
    ps_command(pid, &out)
}

/// `ps` answers in milliseconds. A stuck `ps` must not hold the proxy check (#2923).
const PS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Read the `ps` result for `pid`. `ps -p` exits 1 with no output for a pid that does not exist.
/// Any other failure is an error, so a broken `ps` does not read as "pid gone" (#2923).
pub(crate) fn ps_command(pid: u32, out: &std::process::Output) -> anyhow::Result<Option<String>> {
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        return Ok((!text.is_empty()).then_some(text));
    }
    if out.status.code() == Some(1) && text.is_empty() && out.stderr.is_empty() {
        return Ok(None);
    }
    anyhow::bail!(
        "/bin/ps failed to read the command of pid {pid} ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )
}
