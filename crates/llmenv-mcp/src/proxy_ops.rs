//! Who started the local mcp-proxy, and how to restart it safely (#2417).
//!
//! The proxy log records one line for each spawn, so a later unexplained shutdown can be tied
//! to a spawner. The restart stops one process: the pid in the pidfile, and only when its
//! command line is an `mcp-proxy` for `icm serve`. `pkill -f` stopped every such proxy on the
//! machine. Design: docs/design/issue-2417-mcp-proxy-shutdown.md

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::pid_state::{PidState, pid_state};
use crate::proxy::{log_path_for, open_proxy_log, read_pidfile};
use crate::proxy_command::{command_of, is_proxy_command};
use anyhow::Context as _;

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
    /// The pid in the pidfile is a running process of another user. It is not this user's to
    /// stop, so llmenv does not signal it (#3835).
    OtherUser,
    /// The pid runs, but its command line is not the proxy. Carries that command line.
    NotProxy(String),
    /// The proxy took SIGTERM and exited.
    Stopped,
    /// The proxy still runs after the wait.
    StillRunning,
}

/// The outcome of sending SIGTERM. EPERM means the pid is not one that this user may signal. It
/// is a process of another user, so the pid was reused after the liveness check (#3810), or it is
/// gone. A process of this user that gives EPERM is a sandbox that blocks signals, and that fails
/// loud, because the proxy still runs (#3835). `None` means the signal was sent.
fn sigterm_result(
    sent: rustix::io::Result<()>,
    target: rustix::process::Pid,
) -> anyhow::Result<Option<StopOutcome>> {
    let pid = target.as_raw_nonzero();
    match sent {
        Ok(()) => Ok(None),
        Err(rustix::io::Errno::PERM) => match pid_state(target).with_context(|| {
            format!(
                "the system refused SIGTERM (EPERM) for mcp-proxy (pid {pid}), and llmenv cannot \
                 tell whose process it is. Fix: check that /bin/ps works, then run the restart \
                 again"
            )
        })? {
            PidState::Gone => Ok(Some(StopOutcome::Gone)),
            PidState::OtherUser => Ok(Some(StopOutcome::OtherUser)),
            PidState::Own => Err(anyhow::anyhow!(
                "the system refused SIGTERM (EPERM) for mcp-proxy (pid {pid}), which is a process \
                 of this user. A sandbox that blocks signals does this. Stop the proxy from \
                 outside the sandbox."
            )),
        },
        // The proxy exited between the check and the signal.
        Err(rustix::io::Errno::SRCH) => Ok(Some(StopOutcome::Gone)),
        Err(e) => Err(anyhow::anyhow!(
            "cannot send SIGTERM to mcp-proxy (pid {pid}): {e}. Fix: stop the proxy by hand, \
             then run the restart again"
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
    let state = pid_state(target)
        .with_context(|| format!("cannot tell whether pid {pid} in the pidfile is the proxy"))?;
    match state {
        PidState::Gone => return Ok(StopOutcome::Gone),
        PidState::OtherUser => return Ok(StopOutcome::OtherUser),
        PidState::Own => {}
    }
    let Some(command) = command_of(pid)? else {
        return Ok(StopOutcome::Gone);
    };
    if !is_proxy_command(&command) {
        return Ok(StopOutcome::NotProxy(command));
    }
    if let Some(outcome) = sigterm_result(
        rustix::process::kill_process(target, rustix::process::Signal::TERM),
        target,
    )? {
        return Ok(outcome);
    }
    let start = Instant::now();
    while start.elapsed() < wait {
        // A pid that another user holds now is a pid reused after the proxy exited.
        let state = pid_state(target).with_context(|| {
            format!(
                "SIGTERM was sent to mcp-proxy (pid {pid}), and llmenv cannot tell whether it \
                 stopped. Fix: check that /bin/ps works, then run the restart again"
            )
        })?;
        if state != PidState::Own {
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
    use llmenv_util::proc_start::ps_field;
    use proptest::prelude::*;

    fn pid(raw: i32) -> rustix::process::Pid {
        rustix::process::Pid::from_raw(raw).unwrap()
    }

    /// A pid that no process holds: it is above the pid limit of every system that we run on.
    fn gone_pid() -> rustix::process::Pid {
        pid(i32::MAX)
    }

    #[test]
    fn a_term_refused_with_eperm_is_settled_by_whose_process_it_is() {
        let eperm = || Err(rustix::io::Errno::PERM);
        assert!(matches!(
            sigterm_result(eperm(), gone_pid()),
            Ok(Some(StopOutcome::Gone))
        ));
        if !rustix::process::geteuid().is_root() {
            // Pid 1 belongs to root, so the pid was reused by another user's process.
            assert!(matches!(
                sigterm_result(eperm(), pid(1)),
                Ok(Some(StopOutcome::OtherUser))
            ));
        }
        // This user's own process that refuses a signal is a sandbox, and it fails loud (#3835).
        let own = pid(std::process::id().cast_signed());
        let err = sigterm_result(eperm(), own).unwrap_err().to_string();
        assert!(err.contains("sandbox"), "{err}");
        assert!(matches!(sigterm_result(Ok(()), own), Ok(None)));
        assert!(sigterm_result(Err(rustix::io::Errno::INVAL), own).is_err());
        // A proxy that exits between the check and the signal is gone, not an error.
        assert!(matches!(
            sigterm_result(Err(rustix::io::Errno::SRCH), own),
            Ok(Some(StopOutcome::Gone))
        ));
    }

    /// Pid 1 belongs to root. A process of another user is not running for this user, so the
    /// stop must not fail on it (#3810).
    #[test]
    fn a_process_of_another_user_is_not_running_for_this_user() {
        let pid = rustix::process::Pid::from_raw(1).unwrap();
        assert_ne!(pid_state(pid).unwrap(), PidState::Own);
    }

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
            let read = ps_field(7, &ps_output(0, &out, &err), "command").unwrap();
            let trimmed = out.trim();
            prop_assert_eq!(read, (!trimmed.is_empty()).then(|| trimmed.to_string()));
        }

        #[test]
        fn a_failed_ps_never_panics(code in 1i32..128, out in "\\PC{0,20}", err in "\\PC{0,20}") {
            let _ = ps_field(7, &ps_output(code, &out, &err), "command");
        }
    }

    proptest! {
        /// A failed `ps` reads as a gone pid only for exit 1 with no output. Any exit code of 2
        /// or more is an error, whatever `ps` printed (#2923).
        #[test]
        fn only_a_quiet_exit_1_reads_as_a_gone_pid(
            code in 0i32..128,
            out in "[ a-z]{0,4}",
            err in "[a-z]{0,4}",
        ) {
            let read = ps_field(7, &ps_output(code, &out, &err), "command");
            if code >= 2 {
                prop_assert!(read.is_err());
            }
            if code == 1 && (!out.trim().is_empty() || !err.is_empty()) {
                prop_assert!(read.is_err());
            }
        }
    }

    #[test]
    fn a_ps_failure_is_an_error_and_not_a_gone_pid() {
        assert_eq!(
            ps_field(7, &ps_output(0, "icm serve\n", ""), "command").unwrap(),
            Some("icm serve".to_string())
        );
        assert_eq!(ps_field(7, &ps_output(1, "", ""), "command").unwrap(), None);
        let err = ps_field(7, &ps_output(1, "", "ps: bad option"), "command").unwrap_err();
        assert!(format!("{err:#}").contains("bad option"), "{err:#}");
        assert!(ps_field(7, &ps_output(2, "", ""), "command").is_err());
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
