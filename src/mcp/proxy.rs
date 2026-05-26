//! Lifecycle management for `mcp-proxy`.
//!
//! When this host is the ICM server, the shell hook calls
//! [`ensure_running`] on every export. It re-uses an existing proxy when the
//! pidfile points at a live process and spawns a new one otherwise.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A live proxy already owned the pidfile; nothing was done.
    AlreadyRunning,
    /// A new proxy was spawned and the pidfile was (over)written.
    Spawned,
}

/// Ensures that `mcp-proxy` is running, bound to `bind`. Reads `pid_path` to
/// check for an existing instance; if alive, returns
/// [`EnsureOutcome::AlreadyRunning`]. Otherwise calls `spawn(bind)` and writes
/// the returned pid to `pid_path`.
///
/// `spawn` is injected so tests can simulate process launches without
/// actually invoking `mcp-proxy`. Production callers pass [`spawn_mcp_proxy`].
///
/// # Errors
/// Returns an error if the pidfile contents cannot be parsed, the parent
/// directory cannot be created, the spawn callback fails, or writing the
/// pidfile fails.
pub fn ensure_running<F>(bind: &str, pid_path: &Path, spawn: F) -> anyhow::Result<EnsureOutcome>
where
    F: FnOnce(&str) -> anyhow::Result<u32>,
{
    if let Some(existing) = read_pidfile(pid_path)?
        && is_alive(existing)
    {
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pid = spawn(bind)?;
    std::fs::write(pid_path, pid.to_string())?;
    Ok(EnsureOutcome::Spawned)
}

/// Default path for the proxy pidfile — `$XDG_STATE_HOME/llmenv/mcp-proxy.pid`,
/// falling back to `~/.local/state/llmenv/mcp-proxy.pid`.
#[must_use]
pub fn default_pid_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("llmenv").join("mcp-proxy.pid");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".local/state/llmenv")
            .join("mcp-proxy.pid");
    }
    PathBuf::from(".local/state/llmenv/mcp-proxy.pid")
}

/// Production spawner: launches `mcp-proxy --port <port> -- icm mcp-server`
/// and returns its pid. `bind` is `host:port`; only the port is forwarded to
/// `mcp-proxy` (the proxy binds to all interfaces by default — we trust the
/// network scope to gate access).
///
/// # Errors
/// Returns an error if `bind` has no `:port` suffix or the child cannot be
/// spawned.
pub fn spawn_mcp_proxy(bind: &str) -> anyhow::Result<u32> {
    let port = bind
        .rsplit_once(':')
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow::anyhow!("bind missing :port suffix: {bind}"))?;
    let child = Command::new("mcp-proxy")
        .arg("--port")
        .arg(port)
        .arg("--")
        .arg("icm")
        .arg("mcp-server")
        .spawn()?;
    Ok(child.id())
}

fn read_pidfile(pid_path: &Path) -> anyhow::Result<Option<u32>> {
    if !pid_path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(pid_path)?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let pid: u32 = trimmed
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid pid {trimmed:?} in {}: {e}", pid_path.display()))?;
    Ok(Some(pid))
}

/// True if `pid` is a live process. On Unix this sends signal 0 (no-op delivery
/// check). On other platforms it conservatively returns false so callers
/// always re-spawn.
#[must_use]
pub fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(2) with sig=0 performs only the permission/existence
        // check and never delivers a signal. pid is forwarded as i32 via
        // libc::pid_t; we cap at i32::MAX to avoid wrap.
        let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
        // We avoid pulling libc as a dependency by going through std::process
        // — but std doesn't expose signal-0. Shell-out is reliable and cheap.
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
        let _ = pid;
        false
    }
}
