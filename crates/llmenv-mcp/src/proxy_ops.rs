//! Who started the local mcp-proxy, and how to restart it safely (#2417).
//!
//! The proxy log records one line for each spawn, so a later unexplained shutdown can be tied
//! to a spawner. The restart stops one process: the pid in the pidfile, and only when its
//! command line is an `mcp-proxy` for `icm serve`. `pkill -f` stopped every such proxy on the
//! machine. Design: docs/design/issue-2417-mcp-proxy-shutdown.md

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::proxy::{log_path_for, open_proxy_log, read_pidfile};

/// Which llmenv path started the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnSource {
    /// `llmenv export`, run from a shell prompt.
    Export,
    /// The SessionStart health check of a Claude Code session.
    SessionStart,
    /// `llmenv doctor --restart-memory-proxy`.
    Restart,
}

impl SpawnSource {
    fn label(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::SessionStart => "session-start",
            Self::Restart => "restart",
        }
    }
}

/// The facts about the process that starts the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spawner {
    parent: u32,
    session: Option<u32>,
    group: Option<u32>,
    tty: bool,
}

impl Spawner {
    fn current() -> Self {
        use std::io::IsTerminal;
        Self {
            parent: std::process::id(),
            session: rustix::process::getsid(None)
                .ok()
                .map(|p| p.as_raw_nonzero().get().unsigned_abs()),
            group: Some(rustix::process::getpgid(None))
                .and_then(Result::ok)
                .map(|p| p.as_raw_nonzero().get().unsigned_abs()),
            tty: std::io::stdin().is_terminal(),
        }
    }
}

fn attribution_line(
    pid: u32,
    source: SpawnSource,
    spawner: &Spawner,
    unix_secs: Option<u64>,
) -> String {
    let id = |v: Option<u32>| v.map_or_else(|| "unknown".to_string(), |n| n.to_string());
    let at = unix_secs.map_or_else(|| "unknown".to_string(), |n| n.to_string());
    format!(
        "llmenv: started mcp-proxy pid={pid} source={} at={at} spawner_pid={} \
         spawner_session={} spawner_group={} stdin_tty={}",
        source.label(),
        spawner.parent,
        id(spawner.session),
        id(spawner.group),
        if spawner.tty { "yes" } else { "no" },
    )
}

/// Append the attribution line for a proxy that just started. A failure is not fatal: the line
/// is a diagnostic, and a missing one is smaller than no proxy.
pub(super) fn record_spawn(pid_path: &Path, child_pid: u32, source: SpawnSource) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());
    let line = attribution_line(child_pid, source, &Spawner::current(), now);
    let log_path = log_path_for(pid_path);
    let written = open_proxy_log(&log_path)
        .and_then(|mut log| writeln!(log, "{line}").map_err(anyhow::Error::from));
    if let Err(e) = written {
        // Hook stderr never reaches the agent, so the failure is recorded as a notice (#3204).
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: proxy attribution line not written to {}: {e:#}. Fix: make the proxy log \
             directory writable.",
            log_path.display()
        ));
    }
}

/// What [`stop_proxy`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// No pidfile, or an empty one: llmenv started no proxy that it can name.
    NoPidfile,
    /// The pid in the pidfile is not running.
    Gone,
    /// The pid runs, but its command line is not the proxy. Carries that command line.
    NotProxy(String),
    /// The proxy took SIGTERM and exited.
    Stopped,
    /// The proxy still runs after the wait.
    StillRunning,
}

/// A command line that starts the memory proxy: `mcp-proxy … -- <icm> serve`, where `<icm>` is
/// `icm` or an absolute path to it (#2948). A script runs through its interpreter, and `uvx` runs
/// the tool by name, so `mcp-proxy` is the first or the second word.
fn is_proxy_command(command: &str) -> bool {
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
fn command_of(pid: u32) -> anyhow::Result<Option<String>> {
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
fn ps_command(pid: u32, out: &std::process::Output) -> anyhow::Result<Option<String>> {
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

/// Whether `pid` is a running process that this user can signal.
fn is_running(pid: rustix::process::Pid) -> anyhow::Result<bool> {
    match rustix::process::test_kill_process(pid) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "cannot check pid {}: {e}",
            pid.as_raw_nonzero()
        )),
    }
}

/// Send SIGTERM to the proxy named by the pidfile, and wait up to `wait` for it to exit.
///
/// The command line check guards against pid reuse: the pid may belong to another process now.
///
/// # Errors
/// The pidfile cannot be read or parsed, or the signal cannot be sent.
pub fn stop_proxy(pid_path: &Path, wait: Duration) -> anyhow::Result<StopOutcome> {
    let pid = match read_pidfile(pid_path) {
        Ok(Some(pid)) => pid,
        Ok(None) => return Ok(StopOutcome::NoPidfile),
        Err(e) => anyhow::bail!("cannot read {}: {e}", pid_path.display()),
    };
    let target = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| anyhow::anyhow!("pid {pid} in {} is not a process", pid_path.display()))?;
    if !is_running(target)? {
        return Ok(StopOutcome::Gone);
    }
    let Some(command) = command_of(pid)? else {
        return Ok(StopOutcome::Gone);
    };
    if !is_proxy_command(&command) {
        return Ok(StopOutcome::NotProxy(command));
    }
    rustix::process::kill_process(target, rustix::process::Signal::TERM)
        .map_err(|e| anyhow::anyhow!("cannot send SIGTERM to mcp-proxy (pid {pid}): {e}"))?;
    let start = Instant::now();
    while start.elapsed() < wait {
        if !is_running(target)? {
            return Ok(StopOutcome::Stopped);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(StopOutcome::StillRunning)
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use crate::proxy::is_alive;
    use proptest::prelude::*;

    fn ps_output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    proptest! {
        #[test]
        fn a_successful_ps_gives_its_trimmed_output(out in "\\PC{0,40}", err in "\\PC{0,20}") {
            let read = ps_command(7, &ps_output(0, &out, &err)).unwrap();
            let trimmed = out.trim();
            prop_assert_eq!(read, (!trimmed.is_empty()).then(|| trimmed.to_string()));
        }

        #[test]
        fn a_failed_ps_never_panics(code in 1i32..128, out in "\\PC{0,20}", err in "\\PC{0,20}") {
            let _ = ps_command(7, &ps_output(code, &out, &err));
        }
    }

    #[test]
    fn a_ps_failure_is_an_error_and_not_a_gone_pid() {
        assert_eq!(
            ps_command(7, &ps_output(0, "icm serve\n", "")).unwrap(),
            Some("icm serve".to_string())
        );
        assert_eq!(ps_command(7, &ps_output(1, "", "")).unwrap(), None);
        let err = ps_command(7, &ps_output(1, "", "ps: bad option")).unwrap_err();
        assert!(format!("{err:#}").contains("bad option"), "{err:#}");
        assert!(ps_command(7, &ps_output(2, "", "")).is_err());
    }

    fn spawner() -> Spawner {
        Spawner {
            parent: 10,
            session: Some(20),
            group: None,
            tty: true,
        }
    }

    #[test]
    fn the_attribution_line_names_the_source_and_the_spawner() {
        for (source, label) in [
            (SpawnSource::Export, "source=export"),
            (SpawnSource::SessionStart, "source=session-start"),
            (SpawnSource::Restart, "source=restart"),
        ] {
            let line = attribution_line(7, source, &spawner(), Some(99));
            assert_eq!(
                line,
                format!(
                    "llmenv: started mcp-proxy pid=7 {label} at=99 spawner_pid=10 \
                     spawner_session=20 spawner_group=unknown stdin_tty=yes"
                )
            );
        }
    }

    #[test]
    fn an_unknown_time_is_written_as_unknown() {
        let line = attribution_line(7, SpawnSource::Export, &spawner(), None);
        assert!(line.contains("at=unknown"), "{line}");
    }

    #[test]
    fn only_a_proxy_for_icm_serve_is_a_proxy_command() {
        for (cmd, ok) in [
            ("mcp-proxy --host 0.0.0.0 --port 9092 -- icm serve", true),
            (
                "/home/u/.local/bin/mcp-proxy --host ::1 --port 1 -- icm serve  ",
                true,
            ),
            ("uvx mcp-proxy --host 1 --port 2 -- icm serve", true),
            (
                "/venv/bin/python3 /venv/bin/mcp-proxy --host 1 --port 2 -- icm serve",
                true,
            ),
            (
                "mcp-proxy --host 1 --port 2 -- /opt/homebrew/bin/icm serve",
                true,
            ),
            ("mcp-proxy --host 1 --port 2 -- /x/icm-fake serve", false),
            ("mcp-proxy --host 1 --port 2 icm serve", false),
            ("sh -c echo mcp-proxy -- icm serve", false),
            ("/bin/other /x/mcp-proxy-fake --host 1 -- icm serve", false),
            ("mcp-proxy --host 1 --port 2 -- other serve", false),
            ("vim notes-about-mcp-proxy.txt", false),
            ("icm serve", false),
            ("", false),
        ] {
            assert_eq!(is_proxy_command(cmd), ok, "{cmd:?}");
        }
    }

    #[test]
    fn record_spawn_appends_one_line_to_the_proxy_log() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("mcp-proxy.pid");
        record_spawn(&pid_path, 4242, SpawnSource::SessionStart);
        record_spawn(&pid_path, 4343, SpawnSource::Export);
        let log = std::fs::read_to_string(log_path_for(&pid_path)).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].contains("pid=4242 source=session-start"), "{log}");
        assert!(lines[1].contains("pid=4343 source=export"), "{log}");
    }

    #[test]
    fn stopping_without_a_live_pid_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("mcp-proxy.pid");
        let wait = Duration::from_millis(200);
        assert_eq!(stop_proxy(&pid_path, wait).unwrap(), StopOutcome::NoPidfile);
        std::fs::write(&pid_path, "").unwrap();
        assert_eq!(stop_proxy(&pid_path, wait).unwrap(), StopOutcome::NoPidfile);
        std::fs::write(&pid_path, "not a pid").unwrap();
        assert!(stop_proxy(&pid_path, wait).is_err());
        // A pid that is far above any real one is not running.
        std::fs::write(&pid_path, "2147483000").unwrap();
        assert_eq!(stop_proxy(&pid_path, wait).unwrap(), StopOutcome::Gone);
    }

    #[test]
    fn stopping_refuses_a_process_that_is_not_the_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("mcp-proxy.pid");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(&pid_path, child.id().to_string()).unwrap();
        let outcome = stop_proxy(&pid_path, Duration::from_millis(200)).unwrap();
        assert!(
            matches!(&outcome, StopOutcome::NotProxy(c) if c.contains("sleep")),
            "{outcome:?}"
        );
        assert_eq!(
            is_alive(child.id()),
            Some(true),
            "the other process is untouched"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn stopping_sends_sigterm_to_the_proxy_and_waits_for_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mcp-proxy");
        std::fs::write(
            &script,
            "#!/bin/sh\ntrap 'kill $pid; exit 143' TERM\nsleep 30 &\npid=$!\nwait $pid\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = std::process::Command::new(&script)
            // The proxy gets an absolute `icm` (#2948), so the stop must still know it.
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                "1",
                "--",
                "/usr/local/bin/icm",
                "serve",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let pid_path = dir.path().join("mcp-proxy.pid");
        std::fs::write(&pid_path, pid.to_string()).unwrap();
        // Reap the child in the background: a zombie still answers `kill -0`.
        let reaper = std::thread::spawn(move || child.wait());
        let outcome = stop_proxy(&pid_path, Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, StopOutcome::Stopped);
        let status = reaper.join().unwrap().unwrap();
        assert!(!status.success());
    }

    proptest! {
        #[test]
        fn a_command_without_the_icm_serve_tail_is_never_a_proxy(cmd in "[a-z -]{0,40}") {
            prop_assume!(!cmd.trim_end().ends_with("-- icm serve"));
            prop_assert!(!is_proxy_command(&cmd));
        }
    }
}
#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod notice_tests;
