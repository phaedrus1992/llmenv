//! Child processes with a deadline.

use std::io::Read;
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often the deadline is checked. A short wait keeps a fast command fast.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a killed child gets to exit. A process in uninterruptible I/O, such as on a hung
/// network mount, ignores the kill, so the wait for it must end too.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Run `cmd` and wait at most `timeout`. The child is killed on a timeout, and the error kind is
/// [`std::io::ErrorKind::TimedOut`].
///
/// The piped stdout and stderr are read on their own threads while the child runs. A child that
/// writes more than the pipe holds would otherwise block on the write and never exit (#2832).
/// The reads also end at the deadline, because a process that the child started can hold the
/// pipes open after the child exits. Only the direct child is killed.
///
/// # Errors
/// The command cannot start, its output cannot be read, or it runs past `timeout`.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = cmd.spawn()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill(&mut child);
                return Err(e);
            }
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            return Err(timed_out(timeout));
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    Ok(Output {
        status,
        stdout: collect(stdout, deadline, timeout)?,
        stderr: collect(stderr, deadline, timeout)?,
    })
}

fn timed_out(timeout: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("no answer within {} ms", timeout.as_millis()),
    )
}

type Drain = Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>;

fn drain(pipe: Option<impl Read + Send + 'static>) -> Drain {
    pipe.map(|mut pipe| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
            // The receiver is gone after a timeout, and then the bytes are not needed.
            let _ = tx.send(result);
        });
        rx
    })
}

fn collect(drain: Drain, deadline: Instant, timeout: Duration) -> std::io::Result<Vec<u8>> {
    let Some(rx) = drain else {
        return Ok(Vec::new());
    };
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(timed_out(timeout)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
            "the thread that read the child output stopped",
        )),
    }
}

fn kill(child: &mut Child) {
    if let Err(e) = child.kill() {
        tracing::error!("cannot kill child process {}: {e}", child.id());
    }
    let grace_end = Instant::now() + KILL_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < grace_end => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                tracing::error!(
                    "child process {} did not exit after a kill; it is left running",
                    child.id()
                );
                return;
            }
            Err(e) => {
                tracing::error!("cannot reap child process {}: {e}", child.id());
                return;
            }
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::run_with_timeout;

    #[test]
    fn a_command_that_runs_too_long_is_killed() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdout(Stdio::piped()).stderr(Stdio::piped());
        let err = run_with_timeout(cmd, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let mut quick = Command::new("echo");
        quick
            .arg("hi")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = run_with_timeout(quick, Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn a_grandchild_that_holds_the_pipe_does_not_block_past_the_deadline() {
        // The shell exits at once, but the background sleep keeps stdout open.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 3 & echo started"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let start = std::time::Instant::now();
        let err = run_with_timeout(cmd, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_larger_than_a_pipe_does_not_block_the_child() {
        // 1 MiB is far past the 64 KiB pipe buffer, so an undrained pipe would hang the child.
        let mut cmd = Command::new("head");
        cmd.args(["-c", "1048576", "/dev/zero"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 1_048_576);
    }
}
