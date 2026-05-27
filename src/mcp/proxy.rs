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
/// Concurrency: a sibling `<pid_path>.lock` file is created with
/// `O_CREAT|O_EXCL`. The first writer wins the lock and does the
/// spawn-and-write; other concurrent callers see `AlreadyExists`, wait briefly
/// for the holder to publish the pid, then re-check and either accept the new
/// pidfile or fail loudly. This prevents the TOCTOU window between
/// "pidfile-empty → spawn → write" that would otherwise let two exports each
/// spawn their own proxy.
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

    // Atomic lock acquisition via O_CREAT|O_EXCL. The lockfile sits next to
    // the pidfile so it shares the same parent directory ACLs.
    let lock_path = lockfile_path(pid_path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(_) => {
            // We hold the lock — do the spawn-and-publish, then drop it.
            let result = (|| -> anyhow::Result<EnsureOutcome> {
                // Re-check inside the lock: another writer may have raced us
                // past the early-out and published a live pid between our
                // check above and our lock acquisition.
                if let Some(existing) = read_pidfile(pid_path)?
                    && is_alive(existing)
                {
                    return Ok(EnsureOutcome::AlreadyRunning);
                }
                let pid = spawn(bind)?;
                write_pidfile_atomic(pid_path, pid)?;
                Ok(EnsureOutcome::Spawned)
            })();
            let _ = std::fs::remove_file(&lock_path);
            result
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another caller is mid-spawn. Trust that it will publish a live
            // pid and re-read; if the pidfile still looks dead, surface that
            // as an error rather than racing again — callers can retry.
            if let Some(existing) = read_pidfile(pid_path)?
                && is_alive(existing)
            {
                Ok(EnsureOutcome::AlreadyRunning)
            } else {
                Err(anyhow::anyhow!(
                    "another process holds {} but has not published a live pid",
                    lock_path.display()
                ))
            }
        }
        Err(e) => Err(e.into()),
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

/// Production spawner: launches `mcp-proxy --port <port> -- icm serve` (or
/// `uvx mcp-proxy ...` when `mcp-proxy` isn't on `PATH`) and returns its pid.
/// `bind` is `host:port`; only the port is forwarded to `mcp-proxy` (the proxy
/// binds to all interfaces by default — we trust the network scope to gate
/// access). `icm serve` is the stdio-only memory daemon it bridges onto the
/// network.
///
/// # Errors
/// Returns an error if `bind` has no `:port` suffix, if neither `mcp-proxy` nor
/// `uvx` is on `PATH`, or if the child cannot be spawned.
pub fn spawn_mcp_proxy(bind: &str) -> anyhow::Result<u32> {
    let port = bind
        .rsplit_once(':')
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow::anyhow!("bind missing :port suffix: {bind}"))?;
    let (program, leading) = mcp_proxy_command()?;
    let child = Command::new(program)
        .args(leading)
        .arg("--port")
        .arg(port)
        .arg("--")
        .arg("icm")
        .arg("serve")
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
        #[expect(
            unused_variables,
            reason = "pid is only used on Unix for the kill(2) signal-0 liveness check"
        )]
        let _ = pid;
        false
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::is_executable;
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
}
