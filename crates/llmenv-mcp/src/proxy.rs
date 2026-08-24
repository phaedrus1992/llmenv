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
/// names a process that is not running) on the cold-start paths. The fast path
/// deliberately skips that: it runs on every shell prompt, and judging a pid
/// costs a fork+exec of `kill` — far more than the probe it would follow — while
/// a stale pid is inert once liveness no longer consults it.
///
/// The pid is written only *after* the bind is confirmed and the child is
/// confirmed still alive, so a child that has *already* exited — losing the port
/// to an orphaned proxy, say — is never recorded as the live listener (#1085).
/// A child that is alive at that check and dies immediately afterwards can still
/// have its pid recorded; that window can't be closed from here, and the next
/// export reconciles it. What matters is that the record is no longer written
/// before the bind is known, which is what made a wrong pid the normal outcome.
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
/// waiting paths without spending the production deadline.
///
/// # Errors
/// See [`ensure_running`].
#[doc(hidden)]
pub fn ensure_running_within<F>(
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
    //
    // Deliberately does not reconcile the pidfile. This runs on every shell
    // prompt on the server host, and `is_alive` costs a fork+exec of `kill`
    // (~7 ms measured) — far more than the loopback probe it would follow. A
    // stale pid is harmless now that nothing reads it for liveness, so it is
    // reconciled on the paths that are already paying for a spawn instead.
    if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    if let Some(parent) = pid_path.parent() {
        llmenv_paths::create_dir_owner_only(parent)
            .with_context(|| format!("creating state directory {}", parent.display()))?;
    }

    // Atomic lock acquisition via O_CREAT|O_EXCL. The lockfile sits next to
    // the pidfile so it shares the same parent directory ACLs.
    let lock_path = lockfile_path(pid_path);
    if try_lock(&lock_path)?.is_some() {
        let result = spawn_and_publish(bind, pid_path, spawn, budget_ms);
        release_lock(&lock_path);
        return result;
    }
    adopt_or_reclaim(bind, pid_path, spawn, budget_ms, &lock_path)
}

/// Takes the spawn lock, recording the holder's pid in it.
///
/// Returns `Ok(None)` when someone else already holds it. The pid is recorded so
/// a lock left behind by an export that died mid-spawn can be recognized as
/// stale — see [`adopt_or_reclaim`].
///
/// # Errors
/// Returns an error if the lockfile can't be created for a reason other than
/// already existing.
fn try_lock(lock_path: &Path) -> anyhow::Result<Option<()>> {
    use std::io::Write as _;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("creating proxy lockfile {}", lock_path.display())));
        }
    };
    // Best-effort: an unrecorded holder is treated as stale, which is the safe
    // direction — it can be reclaimed rather than blocking forever.
    let _ = file.write_all(std::process::id().to_string().as_bytes());
    Ok(Some(()))
}

/// Releases the spawn lock, complaining loudly if it can't.
///
/// A lock that won't release makes every later export fail, so unlike most
/// cleanup here this is not silently discarded.
fn release_lock(lock_path: &Path) {
    if let Err(e) = std::fs::remove_file(lock_path) {
        eprintln!(
            "llmenv: could not release proxy lockfile {} ({e}); remove it manually if \
             mcp-proxy stops starting",
            lock_path.display()
        );
    }
}

/// Reads the pid recorded in the lockfile, if it holds one.
fn lock_holder(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Handles the case where a peer already holds the spawn lock: wait for their
/// proxy, and if it never appears, decide whether the lock is simply busy or was
/// orphaned by an export that died holding it.
///
/// The peer needs the whole bind budget to get its proxy listening, so this polls
/// rather than probing once. Probing once meant the loser of a normal cold-start
/// race almost always lost, and printed an alarming message about a lockfile
/// during entirely correct operation.
///
/// Reclaiming matters because there is no other way out: `llmenv export` runs in
/// the shell's foreground process group, so `^C` (or SIGHUP on a dropped SSH
/// session) during the multi-second wait for a slow `uvx mcp-proxy` kills it with
/// the lockfile in place. Without reclamation every later export failed forever
/// on a file the user had never heard of.
///
/// # Errors
/// Returns an error if the lock is held by a live process and nothing is serving
/// `bind`, if a stale lock can't be removed, or if the reclaimed spawn fails.
fn adopt_or_reclaim<F>(
    bind: &str,
    pid_path: &Path,
    spawn: F,
    budget_ms: u64,
    lock_path: &Path,
) -> anyhow::Result<EnsureOutcome>
where
    F: FnOnce(&str) -> anyhow::Result<Child>,
{
    // Check the holder before waiting on it. A lock whose holder is gone has
    // nothing to wait for, so reclaiming it immediately keeps a wedged lock from
    // costing the full budget on every prompt.
    if let Some(holder) = lock_holder(lock_path).filter(|pid| is_alive(*pid) != Some(false)) {
        if wait_for_port(bind, budget_ms) {
            reconcile_pidfile(pid_path);
            return Ok(EnsureOutcome::AlreadyRunning);
        }
        anyhow::bail!(
            "another llmenv process (pid {holder}) holds {} but nothing is serving {bind} \
             after {budget_ms}ms",
            lock_path.display()
        );
    }

    // No live holder — but its proxy may have outlived it, in which case there is
    // nothing to start.
    if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
        reconcile_pidfile(pid_path);
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    tracing::debug!(
        "reclaiming stale proxy lockfile {} (holder is gone)",
        lock_path.display()
    );
    std::fs::remove_file(lock_path)
        .with_context(|| format!("removing stale proxy lockfile {}", lock_path.display()))?;

    // Single bounded retry: whoever wins the reclaimed lock does the spawn, and
    // the loser is told to retry rather than recursing.
    if try_lock(lock_path)?.is_some() {
        let result = spawn_and_publish(bind, pid_path, spawn, budget_ms);
        release_lock(lock_path);
        return result;
    }
    anyhow::bail!(
        "lost the race to reclaim stale proxy lockfile {}; the next export will retry",
        lock_path.display()
    )
}

/// Polls `bind` until something is serving it or `budget_ms` elapses.
///
/// The child-free counterpart to [`wait_for_bind`], for waiting on a proxy that
/// another process is starting.
fn wait_for_port(bind: &str, budget_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    loop {
        if probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(BIND_POLL_INTERVAL_MS));
    }
}

/// Spawns the proxy and publishes its pid, under the caller-held lockfile.
///
/// Split out of [`ensure_running_within`] so the lockfile is released on every
/// exit path without nesting the whole body in a closure.
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

    match wait_for_bind(bind, &mut child, budget_ms) {
        Ok(BindResult::Bound) => {}
        Ok(BindResult::ChildExited(status)) => {
            anyhow::bail!(
                "mcp-proxy (pid {pid}) exited ({status}) before binding to {bind}{}",
                proxy_log_hint(pid_path)
            );
        }
        Ok(BindResult::TimedOut) => {
            anyhow::bail!(
                "mcp-proxy (pid {pid}) did not bind to {bind} within {budget_ms}ms{}{}",
                reap(&mut child),
                proxy_log_hint(pid_path)
            );
        }
        Err(e) => {
            // We can't tell whether it's running, so don't leave it behind to
            // find out — `llmenv export` runs per prompt and would accumulate one
            // per run.
            let reaped = reap(&mut child);
            return Err(e.context(format!("waiting for mcp-proxy (pid {pid}) to bind{reaped}")));
        }
    }

    // The port is serving — but confirm *our* child is what's serving it before
    // recording its pid (#1085). A child that died into a port already held by
    // an orphaned proxy would otherwise be published as the live listener.
    match child.try_wait() {
        Ok(Some(_)) => {
            reconcile_pidfile(pid_path);
            return Ok(EnsureOutcome::AlreadyRunning);
        }
        Ok(None) => {}
        Err(e) => {
            let reaped = reap(&mut child);
            return Err(anyhow::Error::new(e).context(format!(
                "checking whether mcp-proxy (pid {pid}) is still running{reaped}"
            )));
        }
    }

    write_pidfile_atomic(pid_path, pid)?;
    Ok(EnsureOutcome::Spawned)
}

/// Kills and reaps `child`, returning a fragment naming the outcome for the
/// error being built.
///
/// A child that is still running when we give up on it must not be left behind:
/// it is detached into its own process group with its stdio nulled, so nothing
/// else will clean it up, and `llmenv export` runs on every shell prompt — a
/// proxy that stays alive without ever binding would otherwise accumulate one
/// orphan per prompt, indefinitely.
fn reap(child: &mut Child) -> String {
    match child.kill().and_then(|()| child.wait()) {
        Ok(_) => "; killed it".to_owned(),
        // Already gone between the check and the kill — nothing was leaked.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => String::new(),
        Err(e) => format!(
            "; it is STILL RUNNING (could not kill pid {}: {e})",
            child.id()
        ),
    }
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
        // Only a definite "not running" justifies deleting the record. `None`
        // means the check itself failed, and deleting on a guess would discard a
        // valid pid.
        Ok(Some(pid)) => {
            if is_alive(pid) == Some(false) {
                tracing::debug!("clearing proxy pidfile: pid {pid} is not running");
                let _ = std::fs::remove_file(pid_path);
            }
        }
        Err(PidfileError::Unparseable(e)) => {
            tracing::debug!(
                "clearing unparseable proxy pidfile {}: {e}",
                pid_path.display()
            );
            let _ = std::fs::remove_file(pid_path);
        }
        // An I/O error is not evidence the contents are wrong, and deleting would
        // fail for the same reason. Say so instead — `eprintln!` because the
        // default tracing filter is ERROR-only, so a `warn!` would reach nobody.
        Err(PidfileError::Io(e)) => eprintln!(
            "llmenv: cannot read proxy pidfile {} ({e}); fix its permissions or remove it",
            pid_path.display()
        ),
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
    std::fs::write(&tmp, pid.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, pid_path)
        .inspect_err(|_| {
            // Don't leave the temp file behind on every failed publish.
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("publishing pidfile {}", pid_path.display()))?;
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
fn default_log_path() -> anyhow::Result<PathBuf> {
    Ok(log_path_for(&default_pid_path()?))
}

/// Opens the proxy's stderr log for appending, rotating it to `mcp-proxy.log.1`
/// first if it has reached [`PROXY_LOG_MAX_BYTES`].
fn open_proxy_log(path: &Path) -> anyhow::Result<std::fs::File> {
    open_bounded_log(path, PROXY_LOG_MAX_BYTES, LogDirMode::OwnerOnly)
}

/// Whether [`open_bounded_log`] forces the log's parent directory to
/// `0o700`. A bare `bool` here read as a silent, easy-to-transpose footgun
/// once this became a published crate's public API (security-audit,
/// #1465) — each variant now has to be named at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogDirMode {
    /// Force the parent directory to `0o700`. The right choice for anything
    /// rooted in llmenv's own state tree.
    OwnerOnly,
    /// Create the parent directory (if missing) with the default mode and
    /// leave an existing one's permissions alone. For a directory outside
    /// llmenv's own state tree (e.g. a user-configured
    /// `codebase_memory.index_path` that may be shared with another uid) —
    /// forcing it to `0o700` would silently break that sharing (#1196).
    Inherit,
}

/// Opens `path` for appending as a size-bounded diagnostic log, rotating it to
/// `<path>.1` first if it has reached `max_bytes`. Shared by the mcp-proxy
/// stderr log (`open_proxy_log`, fixed to [`PROXY_LOG_MAX_BYTES`]) and other
/// spawned-child stderr redirection that wants the same "diagnosable but
/// bounded" treatment (#1091) rather than duplicating the hardening below.
///
/// Created `0o600` on Unix: log content can carry details of whatever backend
/// the child talks to, and the mode is set at creation rather than chmod'd
/// after so there is no window in which the file is world-readable.
///
/// `dir_mode` controls whether the parent directory is forced to `0o700` —
/// see [`LogDirMode`]. This is a single call rather than the caller
/// hardening separately and discarding the result: a hardening failure
/// (e.g. `EPERM` chmod'ing a directory owned by another uid) must still
/// fail the whole open, not be swallowed while the log is written into an
/// unhardened directory anyway.
///
/// # Errors
/// Returns an error if the parent directory cannot be created/hardened or the
/// log cannot be opened.
pub fn open_bounded_log(
    path: &Path,
    max_bytes: u64,
    dir_mode: LogDirMode,
) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        if dir_mode == LogDirMode::OwnerOnly {
            llmenv_paths::create_dir_owner_only(parent)
                .with_context(|| format!("creating state directory {}", parent.display()))?;
        } else {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating log directory {}", parent.display()))?;
        }
    }

    // Refuse anything at this path that isn't a plain file: opening a
    // symlink would append the child's stderr to whatever it points at, and
    // a pre-placed FIFO would block the open — hanging the shell prompt on
    // every export. Shared with `session_log::file_sink`'s append-mode
    // writer (#1431), which has the same "can't just replace a symlink"
    // constraint an append does.
    // No `.with_context()` here: `reject_non_regular_file`'s own message
    // already names the path and the reason, and wrapping it would bury
    // that behind a generic "inspecting log ..." in `Display`'s default
    // (non-`{:#}`) output.
    llmenv_paths::reject_non_regular_file(path)?;
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() >= max_bytes => {
            // Single generation: enough to keep the previous failure's trace
            // around without unbounded growth. A failed rotation isn't worth
            // aborting the spawn over — the append below still succeeds,
            // though the size bound then depends on the next attempt.
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("inspecting log {}", path.display())));
        }
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let file = opts
        .open(path)
        .with_context(|| format!("opening log {}", path.display()))?;

    // `mode()` only applies at creation, so a log left behind with looser
    // permissions would keep them. The proxy's stderr can describe the memory
    // backend, so tighten rather than inherit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(meta) = file.metadata()
            && meta.permissions().mode() & 0o777 != 0o600
        {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(file)
}

/// What [`tail_bounded_log`] found.
///
/// "Nothing to show" and "couldn't look" have to stay distinguishable: reporting
/// an unreadable log as though the proxy printed nothing tells the user the
/// opposite of the truth, and sends them to read a file they can't read.
enum LogTail {
    Lines(String),
    Empty,
    Unreadable(std::io::Error),
}

/// Builds the trailing fragment of a startup-failure message: the last few lines
/// of the proxy log, or why they aren't available.
///
/// Replaces the previous message's guesses ("check that the port is free and
/// mcp-proxy is correctly installed"), which named two causes that were both
/// wrong in the incident that prompted #1086 — the real cause was an
/// `ImportError` visible only in the discarded stderr.
fn proxy_log_hint(pid_path: &Path) -> String {
    let log_path = log_path_for(pid_path);
    match tail_bounded_log(&log_path, LOG_TAIL_LINES, LOG_TAIL_BYTES) {
        LogTail::Lines(tail) => format!("; last lines of {}:\n  {tail}", log_path.display()),
        LogTail::Empty => format!("; no output in {} either", log_path.display()),
        LogTail::Unreadable(e) => format!("; cannot read {} ({e})", log_path.display()),
    }
}

/// Reads up to `max_lines` trailing lines from `path`, scanning at most
/// `max_bytes` from the end.
///
/// Never fails — a diagnostic aid must not itself become the error — but does
/// report *why* it has nothing, so the caller can tell "the child was silent"
/// from "I couldn't open the log".
///
/// Decoded lossily: the child's stderr is arbitrary bytes, not guaranteed
/// UTF-8. Control characters are stripped, because these lines are
/// third-party output printed straight to the user's terminal and escape
/// sequences in them would be interpreted rather than shown.
fn tail_bounded_log(path: &Path, max_lines: usize, max_bytes: u64) -> LogTail {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LogTail::Empty,
        Err(e) => return LogTail::Unreadable(e),
    };
    // A stat failure must not be treated as len 0: that would seek to the start
    // and quote the *first* bytes of the log under a "last lines" label.
    let len = match f.metadata() {
        Ok(m) => m.len(),
        Err(e) => return LogTail::Unreadable(e),
    };
    if let Err(e) = f.seek(SeekFrom::Start(len.saturating_sub(max_bytes))) {
        return LogTail::Unreadable(e);
    }
    let mut buf = Vec::new();
    // The bound matters beyond the seek: this is the child's live stderr, so
    // it can grow between the stat and the read.
    if let Err(e) = f.take(max_bytes).read_to_end(&mut buf) {
        return LogTail::Unreadable(e);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .map(sanitize_log_line)
        .filter(|l| !l.trim().is_empty())
        .take(max_lines)
        .collect();
    if lines.is_empty() {
        return LogTail::Empty;
    }
    lines.reverse();
    LogTail::Lines(lines.join("\n  "))
}

/// Strips control characters from a log line before it is printed to a terminal.
///
/// The line is `mcp-proxy`'s stderr — third-party output that can echo request
/// data — and the caller writes it straight to the user's terminal, where escape
/// sequences would be acted on instead of displayed.
fn sanitize_log_line(line: &str) -> String {
    line.chars()
        .map(|c| {
            if c == '\t' || !c.is_control() {
                c
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

/// Builds the `mcp-proxy` invocation, preferring a `mcp-proxy` already on
/// `PATH` and falling back to `uvx mcp-proxy` when it isn't installed. Returns
/// the program's **resolved absolute path** plus its leading args; the caller
/// appends `--port`/target.
///
/// The path, not the bare name, because `execvp` would otherwise redo the `PATH`
/// search with its own rules — and POSIX `execvp` honours an empty `PATH` entry
/// as the working directory, which this lookup deliberately does not (#1390). A
/// bare name would hand back the hijack the lookup just refused.
///
/// # Errors
/// Returns an error when neither `mcp-proxy` nor `uvx` is on `PATH` — the
/// memory backend can't be exposed on the network without one of them.
fn mcp_proxy_command() -> anyhow::Result<(PathBuf, Vec<&'static str>)> {
    // An unset PATH resolves nothing, which is the same conclusion the lookup
    // reaches for a PATH with no match — but they mean different things (a
    // stripped environment vs. nothing installed) and the error below can't tell
    // them apart, so leave the distinction in the log the way the shared
    // resolver's `resolve_on_path` does.
    let path_var = std::env::var_os("PATH").unwrap_or_else(|| {
        tracing::debug!("PATH is unset; cannot resolve `mcp-proxy` or `uvx`");
        std::ffi::OsString::new()
    });
    mcp_proxy_command_in(&path_var)
}

/// [`mcp_proxy_command`] against an explicit `PATH` value, so the preference
/// order is testable without mutating the process environment.
///
/// Uses [`llmenv_paths::resolve_in_path_list`] rather than a local resolver:
/// until #1390 this module carried its own copy, which never picked up the
/// empty-`PATH`-entry guard #1382 added, so `doctor`'s "is mcp-proxy available"
/// and this function's answer could disagree — and a `mcp-proxy` or `uvx` in
/// whatever directory llmenv was run from could be executed.
fn mcp_proxy_command_in(
    path_var: &std::ffi::OsStr,
) -> anyhow::Result<(PathBuf, Vec<&'static str>)> {
    if let Some(proxy) = llmenv_paths::resolve_in_path_list("mcp-proxy", path_var) {
        Ok((proxy, vec![]))
    } else if let Some(uvx) = llmenv_paths::resolve_in_path_list("uvx", path_var) {
        Ok((uvx, vec!["mcp-proxy"]))
    } else {
        Err(anyhow::anyhow!(
            "neither `mcp-proxy` nor `uvx` found on PATH; install one to run the \
             memory server, or disable the `memory` config block"
        ))
    }
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
    let mut cmd = Command::new(&program);
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
            // `eprintln!` rather than `tracing::warn!`: the default subscriber
            // filter is ERROR-only, so a warn here would reach nobody — leaving
            // the user in exactly the state #1086 was filed about, silently.
            // Only stdout feeds `source <(llmenv export)`, so stderr is safe.
            eprintln!(
                "llmenv: proxy stderr log unavailable ({e:#}); starting mcp-proxy with its \
                 stderr discarded, so a startup failure will not be diagnosable"
            );
            Stdio::null()
        }
    };
    configure_detached(&mut cmd, stderr);
    cmd.spawn().with_context(|| {
        format!(
            "spawning `{}` to run mcp-proxy (resolved from PATH)",
            program.display()
        )
    })
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
pub fn detach_process_group(cmd: &mut Command) {
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

/// Why a pidfile couldn't be turned into a pid.
///
/// The two cases call for opposite responses — bad contents should be discarded,
/// an unreadable file must be left alone — so they can't be collapsed into one
/// error.
#[derive(Debug)]
enum PidfileError {
    /// The file was read but doesn't contain a pid.
    Unparseable(String),
    /// The file couldn't be read at all (permissions, I/O).
    Io(std::io::Error),
}

impl std::fmt::Display for PidfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable(s) => write!(f, "{s}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

fn read_pidfile(pid_path: &Path) -> Result<Option<u32>, PidfileError> {
    // #893: a single read that distinguishes NotFound (→ absent) from other I/O
    // errors (→ report), rather than an exists() stat that masked every stat
    // failure (e.g. EACCES) as "no pidfile".
    let s = match std::fs::read_to_string(pid_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PidfileError::Io(e)),
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let pid: u32 = trimmed
        .parse()
        .map_err(|e| PidfileError::Unparseable(format!("invalid pid {trimmed:?}: {e}")))?;
    // pid 0 is not a process: `kill 0` signals the caller's own process group, so
    // a 0 here would turn "signal the proxy" into "signal my own shell".
    if pid == 0 {
        return Err(PidfileError::Unparseable(
            "pid 0 is not a process".to_owned(),
        ));
    }
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

/// Whether `pid` is a live process, via a `kill -0` signal-0 check.
///
/// `Some(true)` alive, `Some(false)` definitely not running, `None` when the
/// check itself couldn't be performed. The three-way answer matters because
/// callers act destructively on "not running": collapsing an unusable `kill` (not
/// on `PATH`, or `fork` failing under process exhaustion) into `false` would make
/// them treat every live process as dead.
///
/// # Note on TOCTOU
/// Subject to PID-reuse races: a recycled pid belonging to an unrelated process
/// answers `Some(true)` even though the proxy is gone (#300). Callers with access
/// to the bind address should prefer [`probe_tcp`], which proves the proxy is
/// actually serving.
///
/// Returns `None` on non-Unix platforms, where there is no `kill` to consult.
#[must_use]
pub fn is_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        // Out-of-range pids can't be asked about: `kill` takes an i32, and
        // clamping would silently probe a different process. `kill -0 0` targets
        // the caller's whole process group, so 0 is not a question either.
        let pid_i32 = i32::try_from(pid).ok()?;
        if pid_i32 <= 0 {
            return None;
        }
        // We avoid pulling libc as a dependency by going through std::process
        // — std doesn't expose kill(2) with sig=0 directly.
        Command::new("kill")
            .arg("-0")
            .arg(pid_i32.to_string())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        #[expect(
            unused_variables,
            reason = "pid is only used on Unix for the kill(2) signal-0 liveness check"
        )]
        let _ = pid;
        None
    }
}

#[cfg(all(test, unix))]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test scaffolding"
)]
mod tests {
    use super::{Command, Path, Stdio, mcp_proxy_command_in};
    use std::os::unix::fs::PermissionsExt;

    // #1481/#1486: shared with `tests/mcp_proxy.rs` via `include!` rather than
    // duplicated by hand — an integration test can't import a lib's
    // `#[cfg(test)]` items, so this is the only way to share it verbatim.
    include!("../tests/support/port_guard.rs");

    /// Writes an executable stub named `name` into `dir`.
    fn stub_binary(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn mcp_proxy_command_prefers_an_installed_mcp_proxy() {
        let dir = tempfile::tempdir().expect("tempdir");
        stub_binary(dir.path(), "mcp-proxy");
        stub_binary(dir.path(), "uvx");

        let (program, args) = mcp_proxy_command_in(dir.path().as_os_str()).expect("resolves");
        assert_eq!(
            program,
            dir.path().join("mcp-proxy"),
            "must return the resolved absolute path, not a bare name for execvp to re-search"
        );
        assert!(args.is_empty(), "a direct mcp-proxy needs no leading args");
    }

    #[test]
    fn mcp_proxy_command_falls_back_to_uvx() {
        let dir = tempfile::tempdir().expect("tempdir");
        stub_binary(dir.path(), "uvx");

        let (program, args) = mcp_proxy_command_in(dir.path().as_os_str()).expect("resolves");
        assert_eq!(program, dir.path().join("uvx"));
        assert_eq!(args, vec!["mcp-proxy"]);
    }

    #[test]
    fn mcp_proxy_command_errors_when_neither_is_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A non-executable file with the right name must not satisfy the lookup.
        let plain = dir.path().join("mcp-proxy");
        std::fs::write(&plain, b"x").expect("write");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let err = mcp_proxy_command_in(dir.path().as_os_str()).expect_err("must not resolve");
        assert!(
            err.to_string().contains("neither `mcp-proxy` nor `uvx`"),
            "error must name both candidates, got: {err}"
        );
    }

    /// #1390: this lookup used to carry its own resolver that honoured an empty
    /// `PATH` entry as "the current directory", so an `mcp-proxy` in whatever
    /// directory llmenv was run from could be executed. The shared resolver
    /// skips empty entries, so an all-empty `PATH` resolves nothing even with a
    /// matching executable sitting in the working directory.
    #[test]
    fn mcp_proxy_command_ignores_empty_path_entries() {
        let cwd = std::env::current_dir().expect("cwd");
        assert!(
            !cwd.join("mcp-proxy").exists(),
            "refusing to clobber an existing ./mcp-proxy"
        );
        stub_binary(&cwd, "mcp-proxy");
        let result = mcp_proxy_command_in(std::ffi::OsStr::new("::"));
        std::fs::remove_file(cwd.join("mcp-proxy")).expect("cleanup");
        assert!(
            result.is_err(),
            "an mcp-proxy in the working directory must not satisfy an empty PATH entry"
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

        let _guard = port_guard();
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

        let _guard = port_guard();
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

        let _guard = port_guard();
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

        let _guard = port_guard();
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

        let _guard = port_guard();
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

        // The child was alive when we gave up on it, so it must have been killed
        // rather than detached and forgotten: `llmenv export` runs per prompt, so
        // leaving it would accumulate one orphan per prompt.
        let pid = spawned.lock().expect("lock").expect("spawn recorded a pid");
        assert!(
            msg.contains("killed it"),
            "error must say the child was killed, got: {msg}"
        );
        assert!(
            super::is_alive(pid) == Some(false),
            "the timed-out child (pid {pid}) must not be left running"
        );
    }

    // #1186: the pidfile's parent directory must be owner-only from creation,
    // matching every other state-dir creation site in the codebase.
    #[cfg(unix)]
    #[test]
    fn ensure_running_creates_pidfile_parent_dir_owner_only() {
        use super::ensure_running_within;
        use std::os::unix::fs::PermissionsExt;

        let _guard = port_guard();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let bind = format!("127.0.0.1:{port}");
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("nested").join("mcp-proxy.pid");

        let _ = ensure_running_within(&bind, &pid_path, |_| Err(anyhow::anyhow!("no spawn")), 50);

        let parent = pid_path.parent().expect("parent");
        let mode = std::fs::metadata(parent)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "pidfile parent dir must be owner-only, got {mode:o}"
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
        assert_eq!(is_alive(dead), Some(false), "test pid must be dead");
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

    // #1186: the log's created parent directory must be owner-only too, not
    // just the log file itself.
    #[cfg(unix)]
    #[test]
    fn open_proxy_log_creates_parent_dir_owner_only() {
        use super::open_proxy_log;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("nested").join("mcp-proxy.log");
        drop(open_proxy_log(&log).expect("open"));

        let mode = std::fs::metadata(log.parent().expect("parent"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "log dir must be owner-only, got {mode:o}");
    }

    /// A symlink at the log path must be refused, not followed: opening it would
    /// append the proxy's stderr to whatever an attacker pointed it at.
    #[cfg(unix)]
    #[test]
    fn open_proxy_log_refuses_a_symlink() {
        use super::open_proxy_log;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("victim");
        std::fs::write(&target, b"original\n").expect("write");
        let log = dir.path().join("mcp-proxy.log");
        std::os::unix::fs::symlink(&target, &log).expect("symlink");

        let msg = open_proxy_log(&log)
            .expect_err("a symlink must be refused")
            .to_string();

        assert!(
            msg.contains("not a regular file"),
            "error must say why it refused, got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "original\n",
            "the symlink target must not be written through"
        );
    }

    /// A FIFO at the log path must be refused rather than opened — opening one
    /// blocks until a reader appears, which would hang the shell prompt.
    #[cfg(unix)]
    #[test]
    fn open_proxy_log_refuses_a_fifo() {
        use super::open_proxy_log;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        let made = Command::new("mkfifo")
            .arg(&log)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return; // no mkfifo available
        }

        let msg = open_proxy_log(&log)
            .expect_err("a FIFO must be refused")
            .to_string();

        assert!(
            msg.contains("not a regular file"),
            "error must say why it refused, got: {msg}"
        );
    }

    /// A log left behind with looser permissions is tightened, since `mode()`
    /// only applies when the file is created.
    #[cfg(unix)]
    #[test]
    fn open_proxy_log_tightens_a_loose_existing_log() {
        use super::open_proxy_log;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        std::fs::write(&log, b"old\n").expect("write");
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        drop(open_proxy_log(&log).expect("open"));

        let mode = std::fs::metadata(&log).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an existing loose log must be tightened");
    }

    /// The tail quotes the last lines of the log, skipping blank ones.
    #[test]
    fn tail_proxy_log_returns_trailing_lines() {
        use super::LOG_TAIL_LINES;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        let body: String = (0..LOG_TAIL_LINES + 5)
            .map(|i| format!("line {i}\n\n"))
            .collect();
        std::fs::write(&log, body).expect("write");

        let tail = tail_lines(&log);

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

    /// Unwraps a [`LogTail::Lines`], failing the test on any other variant.
    fn tail_lines(path: &Path) -> String {
        match super::tail_bounded_log(path, super::LOG_TAIL_LINES, super::LOG_TAIL_BYTES) {
            super::LogTail::Lines(s) => s,
            super::LogTail::Empty => panic!("expected lines, got Empty"),
            super::LogTail::Unreadable(e) => panic!("expected lines, got Unreadable({e})"),
        }
    }

    /// A missing or empty log reports `Empty`, not a failure — a diagnostic aid
    /// must never itself become the error.
    #[test]
    fn tail_proxy_log_is_empty_for_missing_or_empty_log() {
        use super::{LOG_TAIL_BYTES, LOG_TAIL_LINES, LogTail, tail_bounded_log};
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            tail_bounded_log(
                &dir.path().join("absent.log"),
                LOG_TAIL_LINES,
                LOG_TAIL_BYTES
            ),
            LogTail::Empty
        ));
        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, b"").expect("write");
        assert!(matches!(
            tail_bounded_log(&empty, LOG_TAIL_LINES, LOG_TAIL_BYTES),
            LogTail::Empty
        ));
        // Whitespace-only is also "the proxy said nothing".
        let blank = dir.path().join("blank.log");
        std::fs::write(&blank, b"\n\n   \n").expect("write");
        assert!(matches!(
            tail_bounded_log(&blank, LOG_TAIL_LINES, LOG_TAIL_BYTES),
            LogTail::Empty
        ));
    }

    /// A log that exists but can't be read must not be reported as "no output" —
    /// that states the opposite of the truth and sends the user to `tail` a file
    /// they have no access to.
    #[cfg(unix)]
    #[test]
    fn tail_proxy_log_distinguishes_unreadable_from_empty() {
        use super::{LOG_TAIL_BYTES, LOG_TAIL_LINES, LogTail, tail_bounded_log};
        use std::fs::Permissions;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        std::fs::write(&log, b"ImportError: boom\n").expect("write");
        std::fs::set_permissions(&log, Permissions::from_mode(0o000)).expect("chmod");

        let result = tail_bounded_log(&log, LOG_TAIL_LINES, LOG_TAIL_BYTES);
        let readable_anyway = std::fs::read(&log).is_ok();
        std::fs::set_permissions(&log, Permissions::from_mode(0o600)).expect("restore");
        if readable_anyway {
            return; // running as root / FS ignores perms
        }

        assert!(
            matches!(result, LogTail::Unreadable(_)),
            "an unreadable log must not be reported as Empty"
        );
    }

    /// Non-UTF-8 stderr is decoded lossily rather than dropped or panicking.
    #[test]
    fn tail_proxy_log_handles_invalid_utf8() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        std::fs::write(&log, b"ImportError: \xff\xfe bad bytes\n").expect("write");

        let tail = tail_lines(&log);

        assert!(
            tail.contains("ImportError"),
            "lossy decode must preserve the readable prefix, got: {tail}"
        );
    }

    /// Escape sequences in the proxy's stderr must not reach the terminal, since
    /// the caller prints these lines to it verbatim.
    #[test]
    fn tail_proxy_log_strips_terminal_escapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("mcp-proxy.log");
        std::fs::write(&log, b"GET /\x1b[2J\x1b]0;pwned\x07 HTTP/1.1\n").expect("write");

        let tail = tail_lines(&log);

        assert!(
            !tail.contains('\u{1b}') && !tail.contains('\u{7}'),
            "control characters must be stripped, got: {tail:?}"
        );
        assert!(
            tail.contains("GET /") && tail.contains("HTTP/1.1"),
            "readable text must survive, got: {tail:?}"
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
