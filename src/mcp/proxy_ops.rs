//! Who started the local mcp-proxy, and how to restart it safely (#2417).
//!
//! The proxy log records one line for each spawn, so a later unexplained shutdown can be tied
//! to a spawner. The restart stops one process: the pid in the pidfile, and only when its
//! command line is an `mcp-proxy` for `icm serve`. `pkill -f` stopped every such proxy on the
//! machine. Design: docs/design/issue-2417-mcp-proxy-shutdown.md

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::mcp::proxy::{is_alive, log_path_for, open_proxy_log, read_pidfile};

/// Which llmenv path started the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnSource {
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

fn attribution_line(pid: u32, source: SpawnSource, spawner: &Spawner, unix_secs: u64) -> String {
    let id = |v: Option<u32>| v.map_or_else(|| "unknown".to_string(), |n| n.to_string());
    format!(
        "llmenv: started mcp-proxy pid={pid} source={} at={unix_secs} spawner_pid={} \
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
        .map_or(0, |d| d.as_secs());
    let line = attribution_line(child_pid, source, &Spawner::current(), now);
    let written = open_proxy_log(&log_path_for(pid_path))
        .and_then(|mut log| writeln!(log, "{line}").map_err(anyhow::Error::from));
    if let Err(e) = written {
        tracing::debug!("proxy attribution line not written: {e:#}");
    }
}

/// What [`stop_proxy`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
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

/// A command line that starts the memory proxy: `mcp-proxy … -- icm serve`, also through `uvx`.
fn is_proxy_command(command: &str) -> bool {
    command.contains("mcp-proxy") && command.trim_end().ends_with("-- icm serve")
}

fn command_of(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Send SIGTERM to the proxy named by the pidfile, and wait up to `wait` for it to exit.
///
/// The command line check guards against pid reuse: the pid may belong to another process now.
///
/// # Errors
/// The pidfile cannot be read or parsed, or the signal cannot be sent.
pub(crate) fn stop_proxy(pid_path: &Path, wait: Duration) -> anyhow::Result<StopOutcome> {
    let pid = match read_pidfile(pid_path) {
        Ok(Some(pid)) => pid,
        Ok(None) => return Ok(StopOutcome::NoPidfile),
        Err(e) => anyhow::bail!("cannot read {}: {e}", pid_path.display()),
    };
    if is_alive(pid) != Some(true) {
        return Ok(StopOutcome::Gone);
    }
    let command = command_of(pid).unwrap_or_default();
    if !is_proxy_command(&command) {
        return Ok(StopOutcome::NotProxy(command));
    }
    let raw = i32::try_from(pid).map_err(|e| anyhow::anyhow!("pid {pid} is out of range: {e}"))?;
    let target = rustix::process::Pid::from_raw(raw)
        .ok_or_else(|| anyhow::anyhow!("pid {pid} is not a process"))?;
    rustix::process::kill_process(target, rustix::process::Signal::TERM)
        .map_err(|e| anyhow::anyhow!("cannot send SIGTERM to mcp-proxy (pid {pid}): {e}"))?;
    let start = Instant::now();
    while start.elapsed() < wait {
        if is_alive(pid) != Some(true) {
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
    use proptest::prelude::*;

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
            let line = attribution_line(7, source, &spawner(), 99);
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
    fn only_a_proxy_for_icm_serve_is_a_proxy_command() {
        for (cmd, ok) in [
            ("mcp-proxy --host 0.0.0.0 --port 9092 -- icm serve", true),
            (
                "/home/u/.local/bin/mcp-proxy --host ::1 --port 1 -- icm serve  ",
                true,
            ),
            ("uvx mcp-proxy --host 1 --port 2 -- icm serve", true),
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
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = std::process::Command::new(&script)
            .args(["--host", "127.0.0.1", "--port", "1", "--", "icm", "serve"])
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
