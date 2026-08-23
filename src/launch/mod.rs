//! `llmenv launch <engine>`: resolve the environment the same way `export`
//! does, then spawn `engine` as a supervised child process with that
//! environment applied on top of the inherited one, inherited stdio, and the
//! child's exit code propagated as `launch`'s own (see #1056).
//!
//! Extracted from `crate::cli` (#1480) so the mid-session supervision work
//! (crash/restart, config-drift and credential-expiry notices) has its own
//! module rather than growing `cli`'s already-largest file further. See
//! `docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md`.

use std::os::unix::process::ExitStatusExt;

use anyhow::Context;

const RELAUNCH_MAX_ATTEMPTS: usize = 3;
const RELAUNCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Caps how many times `launch` will relaunch a crashing child within a
/// rolling window, so a child that crashes on every start doesn't loop
/// forever. Attempts older than [`RELAUNCH_WINDOW`] no longer count.
#[derive(Debug, Default)]
struct RelaunchCap {
    attempts: Vec<std::time::Instant>,
}

impl RelaunchCap {
    /// Record an attempt at `now` and report whether the cap still allows
    /// relaunching (i.e. this attempt was the `RELAUNCH_MAX_ATTEMPTS`-th or
    /// earlier within the window).
    fn record_and_check(&mut self, now: std::time::Instant) -> bool {
        self.attempts
            .retain(|t| now.duration_since(*t) < RELAUNCH_WINDOW);
        self.attempts.push(now);
        self.attempts.len() <= RELAUNCH_MAX_ATTEMPTS
    }
}

/// The scope-narrowing flags `launch` shares with `export` (#1384), bundled so
/// [`run`] stays inside the 5-positional-param limit and so the three
/// always travel to [`crate::cli::resolve_env`] together.
pub(crate) struct LaunchScope {
    pub(crate) scope: Option<String>,
    pub(crate) tag: Option<String>,
    pub(crate) compress: bool,
    pub(crate) auto_restart: bool,
}

/// `llmenv launch <engine>`: resolve the environment the same way `export`
/// does, then spawn `engine` as a supervised child process with that
/// environment applied on top of the inherited one, inherited stdio, and the
/// child's exit code propagated as `launch`'s own (see #1056).
pub(crate) fn run(engine: &str, args: Vec<String>, narrow: LaunchScope) -> anyhow::Result<()> {
    let adapter = crate::adapter::adapter_for_launch_target(engine).ok_or_else(|| {
        anyhow::anyhow!(
            "unrecognized engine '{engine}' — expected one of: {}",
            crate::adapter::registered_adapters()
                .iter()
                .map(|a| a.binary_name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    // One resolution, used both as the "is it installed" gate and as the thing
    // actually spawned — see `command_for_binary`. Resolving PATH directly means
    // a negative result really is a missing engine, not an artifact of `which`
    // being unavailable (#1382).
    let Some(bin_path) = crate::paths::resolve_on_path(adapter.binary_name()) else {
        anyhow::bail!(
            "'{bin}' not found on PATH — install it before running `llmenv launch {engine}`",
            bin = adapter.binary_name()
        );
    };

    let resolved = crate::cli::resolve_env(narrow.scope, narrow.tag, narrow.compress)?;
    let mut cap = RelaunchCap::default();

    loop {
        let mut cmd = crate::cli::command_at_path(&bin_path, adapter.binary_name());
        cmd.args(&args);
        for (key, value) in &resolved.vars {
            cmd.env(key, value);
        }
        cmd.stdin(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());

        let status = crate::cli::run_supervised(cmd, adapter.binary_name(), None)?;

        if status.success() {
            crate::cli::exit_with_status(status);
        }

        let reason = match status.signal() {
            Some(sig) => format!("terminated by signal {sig}"),
            None => format!("exited with code {}", status.code().unwrap_or(-1)),
        };
        eprintln!("llmenv: engine {reason}");

        if !cap.record_and_check(std::time::Instant::now()) {
            eprintln!("llmenv: restart attempts exceeded, giving up");
            crate::cli::exit_with_status(status);
        }

        if narrow.auto_restart {
            eprintln!("llmenv: auto-restarting");
            continue;
        }

        eprint!("Restart? [y/N] ");
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0
            || !answer.trim().eq_ignore_ascii_case("y")
        {
            crate::cli::exit_with_status(status);
        }
    }
}

/// Spawn the engine and wait for it to exit, never dying on a signal itself —
/// `launch`'s exit status must always be the engine's, not a signal it happened
/// to receive first.
///
/// SIGINT and SIGTERM/SIGHUP are treated differently on purpose (#1383):
///
/// - **SIGINT is not forwarded.** The terminal generates it for the entire
///   foreground process group, so the engine already has its own copy, and an
///   agent TUI commonly reads a second interrupt as "force quit" — forwarding
///   would turn one Ctrl-C into two.
/// - **SIGTERM and SIGHUP are forwarded.** A terminal never generates SIGTERM,
///   so one that arrives here came from a supervisor targeting this process by
///   pid — `docker stop` signalling PID 1, systemd `KillMode=mixed`, a CI
///   runner doing `kill <pid>`. The engine would otherwise never learn it
///   should shut down, and nothing would exit until the caller's SIGKILL
///   deadline. Both signals mean "terminate", so the duplicate a rare
///   group-directed kill produces is harmless.
///
/// Either way `launch` keeps waiting afterwards rather than exiting, so the
/// engine gets to shut down and report its own status.
///
/// The handlers are installed *before* the spawn on purpose. Installing them
/// afterwards leaves a window in which a signal kills `launch` under its
/// default disposition while the engine it just started keeps running,
/// orphaning the child and returning a signal-derived status the caller has to
/// interpret as the engine's.
///
/// Unix-only, like `launch` itself — the shipped targets are linux-musl and
/// apple-darwin, and the whole design rests on process-group signal semantics.
pub(crate) async fn spawn_and_supervise(
    cmd: &mut tokio::process::Command,
    binary: &str,
    stdin_payload: Option<&[u8]>,
) -> anyhow::Result<std::process::ExitStatus> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sighup = signal(SignalKind::hangup()).context("failed to install SIGHUP handler")?;

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn '{binary}'"))?;

    // The write runs *inside* the select below rather than before it. Installing
    // the handlers above replaced their default disposition, so from that point
    // on SIGINT/SIGTERM/SIGHUP are only buffered until something calls `recv()` —
    // nothing does until the loop starts. Awaiting the write first meant that a
    // payload larger than the pipe buffer, sent to a child that wasn't draining
    // it, left llmenv blocked and killable by nothing but SIGKILL.
    let mut write = std::pin::pin!(write_stdin_payload(
        child.stdin.take(),
        stdin_payload,
        binary
    ));
    let mut writing = stdin_payload.is_some();

    loop {
        tokio::select! {
            status = child.wait() => {
                return status.context("failed to wait on child engine process");
            }
            result = &mut write, if writing => {
                writing = false;
                if let Err(e) = result {
                    // Deliberately not fatal. A failed write means the child
                    // closed its read end, so it has either exited already or
                    // decided not to read — and its own exit status and stderr
                    // explain that far better than an `EPIPE` here would. Keep
                    // waiting and let the `child.wait()` arm report the real
                    // outcome.
                    //
                    // Returning instead would also drop `child` without killing
                    // it, and a dropped `tokio::process::Child` keeps running —
                    // so the error path of the anti-orphaning fix would orphan the
                    // child. `error!`, not `warn!`: llmenv's default filter is
                    // ERROR-only, and a child that silently never received its
                    // input is not something to leave unexplained.
                    tracing::error!("could not send input to '{binary}': {e:#}");
                }
            }
            _ = sigint.recv() => {
                // Deliberately not forwarded — see the doc comment above.
                tracing::debug!("launch: received SIGINT, still waiting on child");
            }
            _ = sigterm.recv() => {
                forward_signal(&child, rustix::process::Signal::TERM, "SIGTERM");
            }
            _ = sighup.recv() => {
                forward_signal(&child, rustix::process::Signal::HUP, "SIGHUP");
            }
        }
    }
}

/// Write `payload` to `stdin` and close it, or do nothing when there's no
/// payload.
///
/// Takes the handle by value so the write can live in the supervision `select!`
/// without borrowing the `Child` the same `select!` is waiting on.
///
/// # Errors
/// Returns an error when a payload was requested but no stdin pipe was opened for
/// it, or when the write fails — including the `EPIPE` a child that exited before
/// reading produces.
async fn write_stdin_payload(
    stdin: Option<tokio::process::ChildStdin>,
    payload: Option<&[u8]>,
    binary: &str,
) -> anyhow::Result<()> {
    let Some(payload) = payload else {
        return Ok(());
    };
    let Some(stdin) = stdin else {
        anyhow::bail!("'{binary}' was spawned without a stdin pipe to write to");
    };
    write_child_stdin(stdin, payload, binary).await
}

/// Write `payload` to the child's stdin and close the pipe.
///
/// Closing it is the point: `setup`'s crush handoff feeds the skill text on
/// stdin, and crush reads until EOF — leaving the handle open would hang.
///
/// # Errors
/// Returns an error when the write or the close fails. A child that exited before
/// reading its input surfaces here as `EPIPE`, so the message says so rather than
/// reporting a bare "broken pipe".
async fn write_child_stdin(
    mut stdin: tokio::process::ChildStdin,
    payload: &[u8],
    binary: &str,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    stdin.write_all(payload).await.with_context(|| {
        format!("writing to '{binary}' stdin — it may have exited before reading its input")
    })?;
    // `shutdown` flushes and closes; the `stdin` handle is then dropped, so
    // nothing is left holding the write end open.
    stdin
        .shutdown()
        .await
        .with_context(|| format!("closing '{binary}' stdin"))
}

/// Send `signal` to the supervised engine, best-effort.
///
/// Goes through `rustix::process::kill_process` (a direct syscall) rather than
/// fork+exec'ing `kill`, mirroring `consolidation::kill_process_group`. Using
/// the `kill` binary here would reintroduce #1382's failure mode in exactly the
/// distroless-container case this forwarding exists to fix — no `kill` on the
/// image means no shutdown.
///
/// The pid can't have been recycled: `wait` has not completed (this runs from
/// the `select!` arm that races it), so the child is unreaped and its pid is at
/// worst a zombie's.
///
/// Failure is not propagated — the `child.wait()` arm is about to report the
/// engine's real status either way — but anything other than a lost race is
/// logged at `error!`. It has to be `error!` specifically: llmenv's default
/// `EnvFilter` is ERROR-only, so a `warn!` here would be invisible in exactly
/// the situation the user needs it (they ran `docker stop`, forwarding failed,
/// and the engine is still running with no explanation anywhere).
#[cfg(unix)]
fn forward_signal(child: &tokio::process::Child, signal: rustix::process::Signal, name: &str) {
    let Some(raw) = child.id() else {
        tracing::debug!("launch: {name} arrived after the engine exited; nothing to forward");
        return;
    };
    // The remaining guards are should-never-happen: tokio reports a real child
    // pid, which is positive and fits a pid_t. If one ever fires, pid handling
    // upstream is corrupt — a different class of problem from losing a race,
    // and worth surfacing rather than dropping.
    let Ok(raw) = i32::try_from(raw) else {
        tracing::error!("launch: engine pid {raw} does not fit in a pid_t; not forwarding {name}");
        return;
    };
    // Same rule as `consolidation::is_safe_kill_target`: a non-positive pid
    // would mean "my whole process group" or "every process I may signal".
    // `Pid::from_raw` only rejects 0, which this already excludes.
    if raw <= 1 {
        tracing::error!("launch: refusing to forward {name} to pid {raw}");
        return;
    }
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        tracing::error!("launch: engine pid {raw} is not a valid pid; not forwarding {name}");
        return;
    };
    match rustix::process::kill_process(pid, signal) {
        Ok(()) => tracing::debug!("launch: forwarded {name} to the engine"),
        // The engine exited between the pid check above and this syscall —
        // the same benign race as the `child.id()` arm, not worth alarming.
        Err(rustix::io::Errno::SRCH) => {
            tracing::debug!("launch: engine exited before {name} could be forwarded");
        }
        // EPERM here means something (a container security profile, a seccomp
        // filter) is blocking the signal outright, so every later forward will
        // fail too and the engine will never shut down on request.
        Err(e) => {
            tracing::error!(
                "launch: could not forward {name} to the engine: {e}. \
                 The engine may keep running until it is killed directly."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunch_cap_allows_up_to_the_configured_max_within_the_window() {
        let mut cap = RelaunchCap::default();
        let base = std::time::Instant::now();
        assert!(cap.record_and_check(base));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(1)));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(2)));
        // 4th attempt within the window exceeds the cap of 3.
        assert!(!cap.record_and_check(base + std::time::Duration::from_secs(3)));
    }

    #[test]
    fn relaunch_cap_resets_once_attempts_age_out_of_the_window() {
        let mut cap = RelaunchCap::default();
        let base = std::time::Instant::now();
        assert!(cap.record_and_check(base));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(1)));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(2)));
        assert!(!cap.record_and_check(base + std::time::Duration::from_secs(3)));
        // After RELAUNCH_WINDOW has passed since attempt 1, the earlier
        // attempts have aged out and no longer count against the cap.
        let long_after = base + RELAUNCH_WINDOW + std::time::Duration::from_secs(1);
        assert!(cap.record_and_check(long_after));
    }
}
