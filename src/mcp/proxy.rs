//! Lifecycle management for `mcp-proxy`.
//!
//! When this host is the ICM server, the shell hook calls
//! [`ensure_running`] on every export. Liveness is decided by the bind address:
//! if something is serving it, the proxy is up. The pidfile records *which*
//! process to signal, and is never treated as evidence of life (#1085).

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// Something was already serving the bind address; nothing was started.
    AlreadyRunning,
    /// A new proxy was spawned, confirmed bound, and recorded in the pidfile.
    Spawned,
}

/// How long to wait for a TCP connection attempt to the proxy bind address.
///
/// 200 ms is enough for a local loopback bind (typically < 1 ms) while being
/// short enough that a failed check doesn't visibly stall the shell prompt.
const LIVENESS_TCP_TIMEOUT_MS: u64 = 200;

/// How long to wait, in total, for a freshly spawned proxy to open its socket.
///
/// Measured bind times: `mcp-proxy` on `PATH` ~0.55 s, `uvx mcp-proxy` ~2.1 s
/// (it pays uv's resolve cost on top of interpreter startup). A cold uv cache is
/// slower still, so the deadline is generous rather than tight (#1084). It costs
/// nothing in the common failure mode: [`wait_for_bind`] returns as soon as the
/// child exits, so a proxy that dies on startup is reported in milliseconds
/// instead of waiting this out.
const BIND_DEADLINE_MS: u64 = 5_000;

/// How long to wait between bind probes while polling.
///
/// Small enough that a fast bind is not rounded up to a visible delay on the
/// shell prompt, large enough not to spin.
const BIND_POLL_INTERVAL_MS: u64 = 50;

/// Rotate the proxy's stderr log once it reaches this size.
///
/// The log exists to diagnose startup failures, so one generation of history is
/// enough; the cap bounds total on-disk usage at twice this (#1086).
const PROXY_LOG_MAX_BYTES: u64 = 1 << 20;

/// How many trailing lines of the proxy log to quote in a startup-failure error.
const LOG_TAIL_LINES: usize = 10;

/// How many trailing bytes of the proxy log to scan for [`LOG_TAIL_LINES`].
///
/// Bounds the read so a large log can't be pulled into memory wholesale.
const LOG_TAIL_BYTES: u64 = 8 * 1024;

/// Ensures that `mcp-proxy` is running, bound to `bind`. Returns
/// [`EnsureOutcome::AlreadyRunning`] when something is already serving `bind`,
/// otherwise calls `spawn(bind)`, waits for the new child to open its socket,
/// and records its pid in `pid_path`.
///
/// Liveness is decided **only** by attempting a TCP connection to `bind`, never
/// by the pidfile (#1085). A pidfile can be missing while the proxy is up, or
/// name a pid that is dead or recycled; treating it as evidence of life made a
/// stale pidfile unrecoverable and let a dead pid be recorded as the listener.
/// The pidfile answers "who do I signal", and is reconciled (cleared when it
/// names a process that is not running) whenever the port proves a proxy is up.
///
/// The pid is written only *after* the bind is confirmed and the child is
/// confirmed still alive, so a child that dies into a port already held by an
/// orphaned proxy is never recorded as the live listener (#1085).
///
/// Concurrency: a sibling `<pid_path>.lock` file is created with
/// `O_CREAT|O_EXCL`. The first writer wins the lock and does the
/// spawn-and-publish; other concurrent callers see `AlreadyExists`, re-probe the
/// port, and either accept the running proxy or fail loudly. This prevents the
/// TOCTOU window that would otherwise let two exports each spawn their own proxy.
///
/// `spawn` is injected so tests can simulate process launches without actually
/// invoking `mcp-proxy`. Production callers pass [`spawn_mcp_proxy`]. It returns
/// a [`Child`] rather than a bare pid because `Child::try_wait` is the only
/// reliable way to tell a running child from one that has already exited — a
/// `kill -0` check reports an unreaped child as alive (#1085).
///
/// # Errors
/// Returns an error if the parent directory cannot be created, the spawn
/// callback fails, the child exits before binding, the child never binds within
/// [`BIND_DEADLINE_MS`], writing the pidfile fails, or another process holds the
/// lock while nothing is serving `bind`.
pub fn ensure_running<F>(bind: &str, pid_path: &Path, spawn: F) -> anyhow::Result<EnsureOutcome>
where
    F: FnOnce(&str) -> anyhow::Result<Child>,
{
    ensure_running_within(bind, pid_path, spawn, BIND_DEADLINE_MS)
}

/// [`ensure_running`] with an explicit bind budget, so tests can exercise the
/// timeout path without spending the production deadline.
///
/// # Errors
/// See [`ensure_running`].
fn ensure_running_within<F>(
    bind: &str,
    pid_path: &Path,
    spawn: F,
    budget_ms: u64,
) -> anyhow::Result<EnsureOutcome>
where
    F: FnOnce(&str) -> anyhow::Result<Child>,
{
    // Fast path: the port is the source of truth. Something serving `bind` means
    // the proxy is up regardless of what the pidfile says (#1085).
    if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
        reconcile_pidfile(pid_path);
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Atomic lock acquisition via O_CREAT|O_EXCL. The lockfile sits next to
    // the pidfile so it shares the same parent directory ACLs.
    let lock_path = lockfile_path(pid_path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(_) => {
            let result = spawn_and_publish(bind, pid_path, spawn, budget_ms);
            let _ = std::fs::remove_file(&lock_path);
            result
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another caller is mid-spawn. Re-probe the port; if their proxy is
            // up we're done, otherwise surface it rather than racing them.
            if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
                reconcile_pidfile(pid_path);
                Ok(EnsureOutcome::AlreadyRunning)
            } else {
                Err(anyhow::anyhow!(
                    "another process holds {} but nothing is serving {bind}",
                    lock_path.display()
                ))
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Spawns the proxy and publishes its pid, under the caller-held lockfile.
///
/// Split out of [`ensure_running`] so the lockfile is released on every exit
/// path without nesting the whole body in a closure.
fn spawn_and_publish<F>(
    bind: &str,
    pid_path: &Path,
    spawn: F,
    budget_ms: u64,
) -> anyhow::Result<EnsureOutcome>
where
    F: FnOnce(&str) -> anyhow::Result<Child>,
{
    // Re-check inside the lock: another writer may have raced us past the
    // early-out and started a proxy between our probe and our lock acquisition.
    if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
        reconcile_pidfile(pid_path);
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    let mut child = spawn(bind)?;
    let pid = child.id();

    match wait_for_bind(bind, &mut child, budget_ms)? {
        BindResult::Bound => {}
        BindResult::ChildExited(status) => {
            anyhow::bail!(
                "mcp-proxy (pid {pid}) exited ({status}) before binding to {bind}{}",
                proxy_log_hint(pid_path)
            );
        }
        BindResult::TimedOut => {
            anyhow::bail!(
                "mcp-proxy (pid {pid}) did not bind to {bind} within {budget_ms}ms{}",
                proxy_log_hint(pid_path)
            );
        }
    }

    // The port is serving — but confirm *our* child is what's serving it before
    // recording its pid (#1085). A child that died into a port already held by
    // an orphaned proxy would otherwise be published as the live listener.
    if child
        .try_wait()
        .context("waiting on mcp-proxy child")?
        .is_some()
    {
        reconcile_pidfile(pid_path);
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    write_pidfile_atomic(pid_path, pid)?;
    Ok(EnsureOutcome::Spawned)
}

/// Why [`wait_for_bind`] stopped waiting.
#[derive(Debug)]
enum BindResult {
    /// Something is now serving the bind address.
    Bound,
    /// The child exited before the bind address became reachable.
    ChildExited(ExitStatus),
    /// The child is still running but never bound within the deadline.
    TimedOut,
}

/// Polls `bind` until it accepts connections, `child` exits, or `budget_ms`
/// elapses.
///
/// Replaces a single fixed sleep plus one-shot probe (#1084), which both
/// declared failure before a healthy proxy had finished starting and made a
/// genuinely dead proxy take the full settle window to report. The budget is a
/// parameter rather than reading [`BIND_DEADLINE_MS`] directly so the timeout
/// path is testable without spending the production budget.
///
/// # Errors
/// Returns an error only if the child's status cannot be queried.
fn wait_for_bind(bind: &str, child: &mut Child, budget_ms: u64) -> anyhow::Result<BindResult> {
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    loop {
        if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
            return Ok(BindResult::Bound);
        }
        if let Some(status) = child.try_wait().context("waiting on mcp-proxy child")? {
            return Ok(BindResult::ChildExited(status));
        }
        if Instant::now() >= deadline {
            return Ok(BindResult::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(BIND_POLL_INTERVAL_MS));
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
fn reconcile_pidfile(pid_path: &Path) {
    match read_pidfile(pid_path) {
        Ok(None) => {}
        Ok(Some(pid)) if is_alive(pid) => {}
        Ok(Some(pid)) => {
            tracing::debug!("clearing proxy pidfile: pid {pid} is not running");
            let _ = std::fs::remove_file(pid_path);
        }
        Err(e) => {
            tracing::warn!(
                "clearing unreadable proxy pidfile {}: {e}",
                pid_path.display()
            );
            let _ = std::fs::remove_file(pid_path);
        }
    }
}

fn lockfile_path(pid_path: &Path) -> PathBuf {
    let mut s = pid_path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Writes `pid` to `pid_path` atomically via tmpfile + rename. A bare
/// `fs::write` truncates first, so a concurrent reader can observe an empty
/// pidfile mid-write.
fn write_pidfile_atomic(pid_path: &Path, pid: u32) -> anyhow::Result<()> {
    let tmp = pid_path.with_extension(format!("pid.{}.tmp", std::process::id()));
    std::fs::write(&tmp, pid.to_string())?;
    std::fs::rename(&tmp, pid_path)?;
    Ok(())
}

/// Default path for the proxy pidfile — `$XDG_STATE_HOME/llmenv/mcp-proxy.pid`,
/// falling back to `~/.local/state/llmenv/mcp-proxy.pid`.
///
/// # Errors
/// Returns an error if neither `XDG_STATE_HOME` nor `HOME` is set — writing a
/// pidfile to a relative path in the caller's CWD would silently scatter state
/// across whatever directories `llmenv` happens to be invoked from.
pub fn default_pid_path() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("llmenv").join("mcp-proxy.pid"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home)
            .join(".local/state/llmenv")
            .join("mcp-proxy.pid"));
    }
    Err(anyhow::anyhow!(
        "cannot determine pidfile path: neither XDG_STATE_HOME nor HOME is set"
    ))
}

/// Path of the proxy's stderr log, a sibling of the pidfile.
fn log_path_for(pid_path: &Path) -> PathBuf {
    pid_path.with_file_name("mcp-proxy.log")
}

/// Default path for the proxy's stderr log —
/// `$XDG_STATE_HOME/llmenv/mcp-proxy.log`, falling back to
/// `~/.local/state/llmenv/mcp-proxy.log`.
///
/// # Errors
/// Returns an error if neither `XDG_STATE_HOME` nor `HOME` is set.
pub fn default_log_path() -> anyhow::Result<PathBuf> {
    Ok(log_path_for(&default_pid_path()?))
}

/// Opens the proxy's stderr log for appending, rotating it to `mcp-proxy.log.1`
/// first if it has reached [`PROXY_LOG_MAX_BYTES`].
///
/// Created `0o600` on Unix: the proxy's stderr can carry details of the memory
/// backend it bridges, and the mode is set at creation rather than chmod'd after
/// so there is no window in which the file is world-readable.
///
/// # Errors
/// Returns an error if the parent directory cannot be created or the log cannot
/// be opened.
fn open_proxy_log(path: &Path) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= PROXY_LOG_MAX_BYTES) {
        // Single generation: enough to keep the previous failure's trace around
        // without unbounded growth. A failed rotation is not worth aborting the
        // spawn over — the append below still succeeds.
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("opening proxy log {}", path.display()))
}

/// Builds the trailing fragment of a startup-failure message: the last few lines
/// of the proxy log if there are any, otherwise the log's path.
///
/// Replaces the previous message's guesses ("check that the port is free and
/// mcp-proxy is correctly installed"), which named two causes that were both
/// wrong in the incident that prompted #1086 — the real cause was an
/// `ImportError` visible only in the discarded stderr.
fn proxy_log_hint(pid_path: &Path) -> String {
    let log = log_path_for(pid_path);
    let tail = tail_proxy_log(&log);
    if tail.is_empty() {
        format!("; no output in {} either", log.display())
    } else {
        format!("; last lines of {}:\n  {tail}", log.display())
    }
}

/// Reads up to [`LOG_TAIL_LINES`] trailing lines from `path`, scanning at most
/// [`LOG_TAIL_BYTES`] from the end. Returns an empty string when the log is
/// missing, empty, or unreadable — a diagnostic aid must never itself fail.
///
/// Decoded lossily: the proxy's stderr is arbitrary bytes, not guaranteed UTF-8.
fn tail_proxy_log(path: &Path) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut buf = Vec::new();
    if (&mut f).take(LOG_TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(LOG_TAIL_LINES)
        .collect();
    lines.reverse();
    lines.join("\n  ")
}

/// Builds the `mcp-proxy` invocation, preferring a `mcp-proxy` already on
/// `PATH` and falling back to `uvx mcp-proxy` when it isn't installed. Returns
/// the program plus its leading args; the caller appends `--port`/target.
///
/// # Errors
/// Returns an error when neither `mcp-proxy` nor `uvx` is on `PATH` — the
/// memory backend can't be exposed on the network without one of them.
fn mcp_proxy_command() -> anyhow::Result<(&'static str, Vec<&'static str>)> {
    if on_path("mcp-proxy") {
        Ok(("mcp-proxy", vec![]))
    } else if on_path("uvx") {
        Ok(("uvx", vec!["mcp-proxy"]))
    } else {
        Err(anyhow::anyhow!(
            "neither `mcp-proxy` nor `uvx` found on PATH; install one to run the \
             memory server, or disable the `memory` config block"
        ))
    }
}

/// True when `program` resolves to an executable on `PATH`. Scans `$PATH`
/// entries directly rather than shelling out, so it works without a shell and
/// is unaffected by `command`/`which` availability.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Parses a `host:port` bind string into a socket address.
///
/// Delegates to `SocketAddr`'s own parser instead of splitting on the last colon,
/// so this accepts exactly what [`probe_tcp`] accepts. Hand-rolling the split
/// made the two disagree on every IPv6 address — `::1:9092` parsed here but not
/// there, `[::1]:9092` the reverse — so an IPv6 `listen_host` spawned a proxy
/// that the liveness probe could then never see, and every subsequent export
/// spawned another one.
///
/// Parse-don't-validate: the caller reformats the child's argv from the parsed
/// `SocketAddr` rather than passing the original text through, so nothing
/// unvalidated reaches `--host`/`--port`.
///
/// Kept separate from [`spawn_mcp_proxy`] so bind-string parsing is testable
/// without launching a real proxy — exhaustively testing it through the spawner
/// would fork hundreds of short-lived `mcp-proxy` processes and write their
/// stderr into the user's real state directory.
///
/// # Errors
/// Returns an error if `bind` is not an IP literal plus a port. Hostnames are
/// rejected: `mcp-proxy` is given an address to bind, not a name to resolve.
fn parse_bind(bind: &str) -> anyhow::Result<std::net::SocketAddr> {
    bind.parse().map_err(|e| {
        anyhow::anyhow!(
            "bind {bind:?} is not a valid <ip>:<port> address \
             (an IP literal with a port, IPv6 bracketed as [::1]:9092 — not a hostname): {e}"
        )
    })
}

/// Production spawner: launches `mcp-proxy --host <host> --port <port> -- icm serve` (or
/// `uvx mcp-proxy ...` when `mcp-proxy` isn't on `PATH`) and returns the child.
/// `bind` is `host:port` where `host` must be a valid IP address literal;
/// both are forwarded to `mcp-proxy`. `icm serve` is the stdio-only memory
/// daemon it bridges onto the network.
///
/// Returns the [`Child`] rather than its pid so the caller can distinguish a
/// running proxy from one that has already exited; see [`ensure_running`].
///
/// # Errors
/// Returns an error if `bind` has no `:port` suffix, if neither `mcp-proxy` nor
/// `uvx` is on `PATH`, or if the child cannot be spawned.
pub fn spawn_mcp_proxy(bind: &str) -> anyhow::Result<Child> {
    let addr = parse_bind(bind)?;
    let (program, leading) = mcp_proxy_command()?;
    let mut cmd = Command::new(program);
    cmd.args(leading)
        // `--host` takes a bare address; `ip()` drops the brackets an IPv6
        // SocketAddr renders with, which mcp-proxy would reject.
        .arg("--host")
        .arg(addr.ip().to_string())
        .arg("--port")
        .arg(addr.port().to_string())
        .arg("--")
        .arg("icm")
        .arg("serve");
    // Point stderr at the log so a startup failure is diagnosable (#1086). If the
    // log can't be opened we still spawn — a missing diagnostic is a smaller
    // problem than no memory backend — but say so rather than degrading silently.
    let stderr = match default_log_path().and_then(|p| open_proxy_log(&p)) {
        Ok(file) => Stdio::from(file),
        Err(e) => {
            tracing::warn!("proxy stderr log unavailable, discarding proxy stderr: {e}");
            Stdio::null()
        }
    };
    configure_detached(&mut cmd, stderr);
    cmd.spawn().map_err(Into::into)
}

/// Configures `cmd` to run as a detached background daemon rather than a
/// foreground child of the calling shell.
///
/// `llmenv export` is sourced on every prompt via `source <(llmenv export)`,
/// whose process substitution makes the export's stdout the very pipe the shell
/// `source`s. A spawned `mcp-proxy` that inherits these handles writes its log
/// lines straight into that pipe, where the shell then tries to execute them as
/// commands (`command not found: INFO:`) and floods the terminal (#298). It
/// would also be killed by terminal job-control signals (^C / SIGHUP on SSH
/// disconnect) sent to the foreground process group.
///
/// stdin and stdout are always redirected to the null device, which is the part
/// that fixes the pipe pollution — it came from the child inheriting the export's
/// *stdout*, so `stderr` is free to go elsewhere and callers pass it in
/// (#1086 points it at a log file; #298 stays fixed either way). On Unix the
/// child additionally joins a new process group (`process_group(0)`) so
/// foreground-group job-control signals (`^C`) don't reach it; this does *not*
/// start a new session, so a `setsid` daemon would still share the controlling
/// terminal — acceptable here because `llmenv export` exits immediately after
/// spawning, leaving the proxy reparented to init. `setsid` is intentionally not
/// used to avoid pulling in `libc` (mirrors the `is_alive` rationale below).
fn configure_detached(cmd: &mut Command, stderr: Stdio) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    detach_process_group(cmd);
}

/// Joins `cmd`'s child to a new process group (Unix) so foreground job-control
/// signals (^C) don't reach it; see `configure_detached`'s doc comment above
/// for the full terminal-isolation rationale. Split out so callers that need
/// different stdio wiring than `configure_detached`'s null-everything default
/// (e.g. `session_log::detached`, which pipes a JSON payload over stdin) can
/// still share the process-group isolation.
pub(crate) fn detach_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // 0 = make the child its own group leader.
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        // No process-group API in std on non-Unix; only the caller's own
        // stdio redirect applies. Process-group isolation is unavailable here.
        let _ = cmd;
    }
}

fn read_pidfile(pid_path: &Path) -> anyhow::Result<Option<u32>> {
    // #893: a single read that distinguishes NotFound (→ absent) from other I/O
    // errors (→ propagate), rather than an exists() stat that masked every stat
    // failure (e.g. EACCES) as "no pidfile".
    let s = match std::fs::read_to_string(pid_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        r => r.with_context(|| format!("reading {}", pid_path.display()))?,
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let pid: u32 = trimmed
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid pid {trimmed:?} in {}: {e}", pid_path.display()))?;
    Ok(Some(pid))
}

/// Probes `bind` (e.g. `"127.0.0.1:7700"`) by attempting a TCP connection with
/// a `timeout_ms`-millisecond deadline. Returns `true` if the connect succeeds,
/// meaning the proxy has opened its socket and is accepting connections.
///
/// This is the preferred liveness check over `kill -0` because it eliminates
/// the PID-reuse TOCTOU (#300): a recycled PID that belongs to an unrelated
/// process will not be listening on the proxy's port, so the probe correctly
/// returns `false`.
///
/// A failed probe (port not yet open, wrong process on port) returns `false`
/// without surfacing the underlying `io::Error` — callers treat any non-success
/// as "not alive" and act accordingly.
#[must_use]
pub fn probe_tcp(bind: &str, timeout_ms: u64) -> bool {
    use std::net::TcpStream;
    let Ok(addr) = bind.parse::<std::net::SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(timeout_ms)).is_ok()
}

/// True if `pid` is a live process via a `kill -0` signal-0 check.
///
/// # Note on TOCTOU
/// This check is subject to PID-reuse races: a recycled PID that belongs to an
/// unrelated process returns `true` even though the proxy is no longer running
/// (#300). Callers that have access to the bind address should prefer
/// [`probe_tcp`], which proves the proxy is actually serving.
///
/// On non-Unix platforms this conservatively returns `false` so callers always
/// re-spawn.
#[must_use]
pub fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // We avoid pulling libc as a dependency by going through std::process
        // — std doesn't expose kill(2) with sig=0 directly.
        let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
        let status = Command::new("kill")
            .arg("-0")
            .arg(pid_i32.to_string())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) => s.success(),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        #[expect(
            unused_variables,
            reason = "pid is only used on Unix for the kill(2) signal-0 liveness check"
        )]
        let _ = pid;
        false
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{Command, Path, Stdio, is_executable};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn is_executable_true_only_for_executable_files() {
        let dir = tempfile::tempdir().expect("tempdir");

        let exe = dir.path().join("tool");
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(is_executable(&exe), "0o755 file should be executable");

        let plain = dir.path().join("data");
        std::fs::write(&plain, b"x").expect("write");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(
            !is_executable(&plain),
            "0o644 file should not be executable"
        );

        assert!(
            !is_executable(&dir.path().join("missing")),
            "missing path should not be executable"
        );

        assert!(
            !is_executable(dir.path()),
            "a directory should not count as an executable file"
        );
    }

    #[test]
    fn configure_detached_spawns_child_in_new_process_group() {
        use super::configure_detached;
        use std::process::Command;

        // `sleep` is alive long enough to inspect; we kill it before asserting.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        configure_detached(&mut cmd, std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn sleep");
        let child_pid = child.id();

        let pgid = |pid: u32| -> String {
            let out = Command::new("ps")
                .args(["-o", "pgid=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let child_pgid = pgid(child_pid);

        // Clean up before asserting so a failed assertion never leaks the child.
        let _ = child.kill();
        let _ = child.wait();

        // process_group(0) makes the child its own group leader: its pgid equals
        // its own pid. Asserting the exact value (not merely "differs from the
        // parent") pins the documented guarantee — a child merely moved into
        // some other foreign group would not satisfy this (#298).
        assert_eq!(
            child_pgid,
            child_pid.to_string(),
            "configure_detached must make the child its own process-group leader"
        );
    }

    // #893: a non-NotFound I/O error (EACCES) must propagate, not be swallowed
    // as Ok(None) the way the old exists() guard masked stat failures.
    #[cfg(unix)]
    #[test]
    fn read_pidfile_propagates_permission_error() {
        use super::read_pidfile;
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("mcp-proxy.pid");
        fs::write(&path, "123").unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o000)).unwrap();
        let result = read_pidfile(&path);
        let readable_anyway = fs::read_dir(&dir).is_ok();
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap(); // restore for cleanup
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "permission error must propagate, got {result:?}"
        );
    }

    mod props {
        use super::super::{read_pidfile, write_pidfile_atomic};
        use proptest::prelude::*;

        proptest! {
            // Any pid written via the atomic writer reads back unchanged.
            #[test]
            fn pidfile_write_read_roundtrips(pid in any::<u32>()) {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = dir.path().join("mcp-proxy.pid");
                write_pidfile_atomic(&path, pid).expect("write");
                let read = read_pidfile(&path).expect("read");
                prop_assert_eq!(read, Some(pid));
            }

            // Non-numeric pidfile contents are never silently misparsed into a
            // bogus pid: read_pidfile either errors or reports an absent pid
            // (e.g. when the content trims to empty), but never yields Some.
            #[test]
            fn pidfile_parse_never_invents_a_pid(s in "[^0-9]{1,12}") {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = dir.path().join("mcp-proxy.pid");
                std::fs::write(&path, &s).expect("write");
                match read_pidfile(&path) {
                    Ok(None) | Err(_) => {}
                    Ok(Some(pid)) => prop_assert!(false, "parsed bogus pid {pid} from {s:?}"),
                }
            }
        }
    }

    /// probe_tcp returns false for an unparseable or unroutable address (#300).
    /// This is the core property we depend on: a recycled PID that belongs to an
    /// unrelated process will not be listening on the proxy's port.
    #[test]
    fn probe_tcp_returns_false_for_invalid_address() {
        use super::probe_tcp;
        // An unparseable address can never connect.
        assert!(!probe_tcp("not-a-valid-address", 200));
        // Port 0 is never bound by a real server.
        assert!(!probe_tcp("127.0.0.1:0", 200));
    }

    /// probe_tcp returns true when a real TCP listener exists (#300/#301).
    #[test]
    fn probe_tcp_returns_true_for_open_port() {
        use super::probe_tcp;
        use std::net::TcpListener;

        // Bind an ephemeral port to act as the "proxy".
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let bind = addr.to_string();

        assert!(
            probe_tcp(&bind, 200),
            "probe_tcp must return true when a listener is bound on {bind}"
        );
    }

    // `ensure_running`'s own behaviour is covered in tests/mcp_proxy.rs, which
    // owns the ephemeral-port machinery needed to keep those cases from flaking.
    // What follows unit-tests the private helpers around it.

    /// Spawns a child that stays alive but never binds anything.
    fn idle_child() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep")
    }

    /// A closed port plus a live child means "not yet" — [`wait_for_bind`] must
    /// keep waiting and report `TimedOut` only once the budget is spent (#1084).
    #[test]
    fn wait_for_bind_times_out_while_the_child_lives_and_never_binds() {
        use super::{BindResult, wait_for_bind};

        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let bind = format!("127.0.0.1:{port}");
        let mut child = idle_child();

        let started = std::time::Instant::now();
        let result = wait_for_bind(&bind, &mut child, 150);
        let elapsed = started.elapsed();

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            matches!(result, Ok(BindResult::TimedOut)),
            "expected TimedOut, got {result:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(150),
            "must wait out the whole budget rather than probing once, waited {elapsed:?}"
        );
    }

    /// A child that dies without binding is reported immediately, rather than
    /// making the caller wait out the full budget (#1084/#1086).
    #[test]
    fn wait_for_bind_reports_child_exit_without_waiting_out_the_budget() {
        use super::{BindResult, wait_for_bind};

        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let bind = format!("127.0.0.1:{port}");
        let mut child = Command::new("false")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn false");

        let started = std::time::Instant::now();
        let result = wait_for_bind(&bind, &mut child, 10_000);
        let elapsed = started.elapsed();

        match result {
            Ok(BindResult::ChildExited(status)) => {
                assert!(!status.success(), "`false` must report a failing status");
            }
            other => panic!("expected ChildExited, got {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a dead child must short-circuit the budget, took {elapsed:?}"
        );
    }

    /// A bound port returns `Bound` promptly — a fast bind must not be rounded up
    /// to the full settle window the way the old fixed sleep did (#1084).
    #[test]
    fn wait_for_bind_returns_promptly_once_bound() {
        use super::{BindResult, wait_for_bind};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let bind = listener.local_addr().expect("addr").to_string();
        let mut child = idle_child();

        let started = std::time::Instant::now();
        let result = wait_for_bind(&bind, &mut child, 10_000);
        let elapsed = started.elapsed();

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            matches!(result, Ok(BindResult::Bound)),
            "expected Bound, got {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "an already-bound port must return immediately, took {elapsed:?}"
        );
    }

    /// End-to-end timeout path: a child that stays alive but never binds must
    /// fail with the budget and the log path named, and must not leave a pidfile
    /// claiming a proxy that isn't serving (#1084/#1086).
    #[test]
    fn ensure_running_times_out_when_the_child_never_binds() {
        use super::ensure_running_within;

        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let bind = format!("127.0.0.1:{port}");
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");

        let spawned = std::sync::Mutex::new(None);
        let result = ensure_running_within(
            &bind,
            &pid_path,
            |_| {
                let child = idle_child();
                *spawned.lock().expect("lock") = Some(child.id());
                Ok(child)
            },
            150,
        );

        if let Some(pid) = *spawned.lock().expect("lock") {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }

        let msg = result
            .expect_err("must error when the child never binds")
            .to_string();
        assert!(
            msg.contains("did not bind") && msg.contains("150ms"),
            "error must report the exhausted budget, got: {msg}"
        );
        assert!(
            msg.contains("mcp-proxy.log"),
            "error must name the stderr log, got: {msg}"
        );
        assert!(
            !pid_path.exists(),
            "a proxy that never bound must not leave a pidfile"
        );
    }

    /// A pidfile naming a live process is left alone; the running pid is the best
    /// available answer to "who do I signal" (#1085).
    #[test]
    fn reconcile_pidfile_keeps_a_live_pid() {
        use super::{read_pidfile, reconcile_pidfile, write_pidfile_atomic};
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");
        write_pidfile_atomic(&pid_path, std::process::id()).expect("write");

        reconcile_pidfile(&pid_path);

        assert_eq!(
            read_pidfile(&pid_path).expect("read"),
            Some(std::process::id()),
            "a live pid must survive reconciliation"
        );
    }

    /// A pidfile naming a dead process is cleared, so the stale value can't be
    /// mistaken for the listener or persist forever (#1085).
    #[test]
    fn reconcile_pidfile_clears_a_dead_pid() {
        use super::{is_alive, read_pidfile, reconcile_pidfile, write_pidfile_atomic};
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");
        // Above the default pid_max on Linux and macOS, so it cannot be in use.
        let dead = 4_000_003_u32;
        assert!(!is_alive(dead), "test pid must be dead");
        write_pidfile_atomic(&pid_path, dead).expect("write");

        reconcile_pidfile(&pid_path);

        assert_eq!(
            read_pidfile(&pid_path).expect("read"),
            None,
            "a dead pid must be cleared"
        );
    }

    /// An unparseable pidfile is cleared rather than left to fail every later read.
    #[test]
    fn reconcile_pidfile_clears_garbage() {
        use super::reconcile_pidfile;
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");
        std::fs::write(&pid_path, "not-a-pid").expect("write");

        reconcile_pidfile(&pid_path);

        assert!(!pid_path.exists(), "garbage pidfile must be removed");
    }

    /// An absent pidfile is not an error and nothing is created.
    #[test]
    fn reconcile_pidfile_tolerates_absent_file() {
        use super::reconcile_pidfile;
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");

        reconcile_pidfile(&pid_path);

        assert!(!pid_path.exists());
    }

    /// The log is appended to, and rotated to `.log.1` once it passes the cap
    /// so it can't grow without bound (#1086).
    #[test]
    fn open_proxy_log_appends_then_rotates_past_the_cap() {
        use super::{PROXY_LOG_MAX_BYTES, open_proxy_log};
        use std::io::Write as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");

        let mut f = open_proxy_log(&log).expect("open");
        f.write_all(b"first\n").expect("write");
        drop(f);
        let mut f = open_proxy_log(&log).expect("reopen");
        f.write_all(b"second\n").expect("write");
        drop(f);
        assert_eq!(
            std::fs::read_to_string(&log).expect("read"),
            "first\nsecond\n",
            "a below-cap log must be appended to, not truncated"
        );

        // Push it past the cap, then confirm the next open rotates.
        let filler = vec![b'x'; usize::try_from(PROXY_LOG_MAX_BYTES).expect("cap fits usize")];
        std::fs::write(&log, &filler).expect("fill");
        let mut f = open_proxy_log(&log).expect("open after fill");
        f.write_all(b"after rotation\n").expect("write");
        drop(f);

        assert_eq!(
            std::fs::read_to_string(&log).expect("read"),
            "after rotation\n",
            "an over-cap log must be rotated away, leaving a fresh log"
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("mcp-proxy.log.1"))
                .expect("rotated log")
                .len(),
            PROXY_LOG_MAX_BYTES,
            "the previous generation must be preserved as .log.1"
        );
    }

    /// The log is created 0o600 — the proxy's stderr can describe the memory
    /// backend, and the mode is set at creation rather than chmod'd after.
    #[cfg(unix)]
    #[test]
    fn open_proxy_log_is_owner_only() {
        use super::open_proxy_log;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        drop(open_proxy_log(&log).expect("open"));

        let mode = std::fs::metadata(&log).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "proxy log must not be group/world readable");
    }

    /// open_proxy_log creates the state directory rather than failing when the
    /// pidfile's parent doesn't exist yet.
    #[test]
    fn open_proxy_log_creates_missing_parent() {
        use super::open_proxy_log;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("nested").join("mcp-proxy.log");
        drop(open_proxy_log(&log).expect("open"));
        assert!(log.exists());
    }

    /// The tail quotes the last lines of the log, skipping blank ones.
    #[test]
    fn tail_proxy_log_returns_trailing_lines() {
        use super::{LOG_TAIL_LINES, tail_proxy_log};
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        let body: String = (0..LOG_TAIL_LINES + 5)
            .map(|i| format!("line {i}\n\n"))
            .collect();
        std::fs::write(&log, body).expect("write");

        let tail = tail_proxy_log(&log);

        assert_eq!(
            tail.lines().count(),
            LOG_TAIL_LINES,
            "tail must be capped at LOG_TAIL_LINES, got: {tail}"
        );
        assert!(
            tail.contains(&format!("line {}", LOG_TAIL_LINES + 4)),
            "tail must include the final line, got: {tail}"
        );
        assert!(
            !tail.contains("line 0"),
            "tail must not reach back to the first line, got: {tail}"
        );
    }

    /// A missing log yields an empty tail — a diagnostic aid must never fail.
    #[test]
    fn tail_proxy_log_is_empty_for_missing_or_empty_log() {
        use super::tail_proxy_log;
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(tail_proxy_log(&dir.path().join("absent.log")), "");
        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, b"").expect("write");
        assert_eq!(tail_proxy_log(&empty), "");
    }

    /// Non-UTF-8 stderr is decoded lossily rather than dropped or panicking.
    #[test]
    fn tail_proxy_log_handles_invalid_utf8() {
        use super::tail_proxy_log;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        std::fs::write(&log, b"ImportError: \xff\xfe bad bytes\n").expect("write");

        let tail = tail_proxy_log(&log);

        assert!(
            tail.contains("ImportError"),
            "lossy decode must preserve the readable prefix, got: {tail}"
        );
    }

    /// The hint names the log path when there is no output to quote, and drops
    /// the old speculative "port is free / correctly installed" guesses (#1086).
    #[test]
    fn proxy_log_hint_names_the_log_path() {
        use super::proxy_log_hint;
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");

        let hint = proxy_log_hint(&pid_path);

        assert!(
            hint.contains("mcp-proxy.log"),
            "hint must name the log path, got: {hint}"
        );
        assert!(
            !hint.contains("correctly installed") && !hint.contains("port is free"),
            "hint must not restate the old guesses, got: {hint}"
        );
    }

    /// When the log has content, the hint quotes it — this is the payload that
    /// turns an opaque bind failure into a diagnosable one (#1086).
    #[test]
    fn proxy_log_hint_quotes_log_contents() {
        use super::{log_path_for, proxy_log_hint};
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");
        std::fs::write(
            log_path_for(&pid_path),
            b"ImportError: cannot import name 'request_ctx'\n",
        )
        .expect("write log");

        let hint = proxy_log_hint(&pid_path);

        assert!(
            hint.contains("ImportError: cannot import name 'request_ctx'"),
            "hint must quote the proxy's stderr, got: {hint}"
        );
    }

    /// The log sits next to the pidfile, so both resolve under the same state dir.
    #[test]
    fn log_path_is_a_sibling_of_the_pidfile() {
        use super::log_path_for;
        assert_eq!(
            log_path_for(Path::new("/var/state/llmenv/mcp-proxy.pid")),
            Path::new("/var/state/llmenv/mcp-proxy.log")
        );
    }

    /// Malformed bind strings are rejected before they can reach mcp-proxy's
    /// argv: no port, a non-numeric port, and a hostname rather than an IP (#337).
    #[test]
    fn parse_bind_rejects_malformed_addresses() {
        use super::parse_bind;
        for bad in ["127.0.0.1", "127.0.0.1:notaport", "localhost:7878", ""] {
            let msg = parse_bind(bad)
                .expect_err("must reject {bad:?}")
                .to_string();
            assert!(
                msg.contains(bad) && msg.contains("<ip>:<port>"),
                "error should quote the input and name the expected form, got: {msg}"
            );
        }
    }

    /// parse_bind must accept exactly what probe_tcp accepts. They used to
    /// disagree on every IPv6 address: a bare `::1:9092` parsed here (so a proxy
    /// was spawned) but not there (so the probe never saw it), and each export
    /// spawned another one. Both now go through SocketAddr.
    #[test]
    fn parse_bind_agrees_with_probe_tcp_on_ipv6() {
        use super::parse_bind;
        let bracketed = "[::1]:9092";
        let addr = parse_bind(bracketed).expect("bracketed ipv6 must parse");
        assert_eq!(addr.port(), 9092);
        assert_eq!(addr.ip(), "::1".parse::<std::net::IpAddr>().expect("ipv6"));
        assert_eq!(
            addr,
            bracketed
                .parse::<std::net::SocketAddr>()
                .expect("probe_tcp parses the same form"),
            "parse_bind and probe_tcp must agree"
        );

        // The unbracketed form is what `format!(\"{host}:{port}\")` used to
        // produce; it is rejected here so the mismatch can't come back.
        assert!(
            parse_bind("::1:9092").is_err(),
            "unbracketed IPv6 must be rejected — probe_tcp cannot parse it"
        );
    }

    /// `--host` gets a bare address: an IPv6 SocketAddr renders bracketed, and
    /// mcp-proxy would reject `[::1]` as a host.
    #[test]
    fn ipv6_host_argument_is_unbracketed() {
        use super::parse_bind;
        let addr = parse_bind("[::1]:9092").expect("parse");
        assert_eq!(addr.to_string(), "[::1]:9092");
        assert_eq!(addr.ip().to_string(), "::1");
    }

    /// #341: property tests for bind-string parsing.
    ///
    /// These target [`parse_bind`] rather than `spawn_mcp_proxy`. Driving them
    /// through the spawner forked a real `mcp-proxy` per generated case — a few
    /// hundred processes per run, each writing startup noise into the user's
    /// state directory — while testing nothing the pure parser doesn't.
    mod bind_string_props {
        use super::super::parse_bind;
        use proptest::prelude::*;

        proptest! {
            /// Any valid IPv4 + u16 port pair parses, and round-trips to the same
            /// values the child's argv is built from.
            #[test]
            fn valid_ip_port_round_trips(
                a in 0u8..=255,
                b in 0u8..=255,
                c in 0u8..=255,
                d in 0u8..=255,
                port in 1u16..=65535,
            ) {
                let bind = format!("{a}.{b}.{c}.{d}:{port}");
                let addr = parse_bind(&bind)
                    .map_err(|e| TestCaseError::fail(format!("valid bind {bind} rejected: {e}")))?;
                prop_assert_eq!(addr.ip().to_string(), format!("{a}.{b}.{c}.{d}"));
                prop_assert_eq!(addr.port(), port);
            }

            /// Any IPv6 address the config layer accepts must round-trip once the
            /// caller renders it through SocketAddr — the bracketing that the
            /// bare `{host}:{port}` form got wrong.
            #[test]
            fn ipv6_round_trips_through_socket_addr(
                segs in proptest::collection::vec(0u16..=0xffff, 8),
                port in 1u16..=65535,
            ) {
                let octets: [u16; 8] = segs.try_into().map_err(|_| {
                    TestCaseError::fail("strategy must yield exactly 8 segments")
                })?;
                let ip = std::net::IpAddr::from(std::net::Ipv6Addr::from(octets));
                let bind = std::net::SocketAddr::new(ip, port).to_string();
                let addr = parse_bind(&bind)
                    .map_err(|e| TestCaseError::fail(format!("valid bind {bind} rejected: {e}")))?;
                prop_assert_eq!(addr.ip(), ip);
                prop_assert_eq!(addr.port(), port);
            }

            /// parse_bind and probe_tcp must never disagree about what a bind
            /// address is. Anything one accepts, the other must accept.
            #[test]
            fn parse_bind_matches_socket_addr_exactly(s in "[0-9a-fA-F:.\\[\\]]{1,30}") {
                let ours = parse_bind(&s).is_ok();
                let theirs = s.parse::<std::net::SocketAddr>().is_ok();
                prop_assert_eq!(
                    ours, theirs,
                    "parse_bind and SocketAddr disagree on {:?} (parse_bind={}, SocketAddr={})",
                    s, ours, theirs
                );
            }

            /// Arbitrary strings without a colon must always produce a parse error.
            #[test]
            fn no_colon_always_errors(s in "[a-zA-Z0-9]{1,20}") {
                prop_assert!(
                    parse_bind(&s).is_err(),
                    "must reject bind without a port: {}", s
                );
            }

            /// A non-numeric port is always rejected, never coerced into a number.
            #[test]
            fn non_numeric_port_always_errors(s in "[a-zA-Z]{1,8}") {
                let bind = format!("127.0.0.1:{s}");
                prop_assert!(parse_bind(&bind).is_err(), "must reject port {:?}", s);
            }
        }
    }
}
