//! Lifecycle management for `mcp-proxy`.
//!
//! When this host is the ICM server, the shell hook calls
//! [`ensure_running`] on every export. It re-uses an existing proxy when the
//! pidfile points at a live process and spawns a new one otherwise.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A live proxy already owned the pidfile; nothing was done.
    AlreadyRunning,
    /// A new proxy was spawned and the pidfile was (over)written.
    Spawned,
}

/// How long to wait for a TCP connection attempt to the proxy bind address.
///
/// 200 ms is enough for a local loopback bind (typically < 1 ms) while being
/// short enough that a failed check doesn't visibly stall the shell prompt.
const LIVENESS_TCP_TIMEOUT_MS: u64 = 200;

/// How long to wait after spawning before probing the proxy's TCP port.
///
/// The proxy needs a moment to open its listening socket. 300 ms covers the
/// typical interpreter startup + bind time on a busy machine without noticeably
/// delaying `llmenv export`.
const SPAWN_SETTLE_MS: u64 = 300;

/// Ensures that `mcp-proxy` is running, bound to `bind`. Reads `pid_path` to
/// check for an existing instance; if alive, returns
/// [`EnsureOutcome::AlreadyRunning`]. Otherwise calls `spawn(bind)` and writes
/// the returned pid to `pid_path`.
///
/// Liveness is checked by attempting a TCP connection to `bind`. This is more
/// reliable than a PID-existence check (`kill -0`): it proves the proxy has
/// opened its socket and is accepting connections, not just that *some* process
/// holds the PID (PID-reuse TOCTOU, #300). A post-spawn probe also surfaces
/// bind/startup failures that were previously invisible after stderr was
/// silenced (#301).
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
/// directory cannot be created, the spawn callback fails, writing the pidfile
/// fails, or the proxy does not become reachable within the settle window.
pub fn ensure_running<F>(bind: &str, pid_path: &Path, spawn: F) -> anyhow::Result<EnsureOutcome>
where
    F: FnOnce(&str) -> anyhow::Result<u32>,
{
    // Fast path: existing proxy is already accepting connections (#300 — TCP
    // probe rather than kill-0 avoids the PID-reuse TOCTOU).
    if read_pidfile(pid_path)?.is_some() && probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
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
                // check above and our lock acquisition (#300).
                if read_pidfile(pid_path)?.is_some() && probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
                    return Ok(EnsureOutcome::AlreadyRunning);
                }
                let pid = spawn(bind)?;
                write_pidfile_atomic(pid_path, pid)?;

                // Post-spawn liveness check (#301): give the proxy time to bind
                // its socket, then verify it's actually accepting connections.
                // This surfaces startup failures (bad port, missing binary, etc.)
                // that were previously invisible after stderr was silenced.
                std::thread::sleep(std::time::Duration::from_millis(SPAWN_SETTLE_MS));
                if !probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
                    let _ = std::fs::remove_file(pid_path);
                    anyhow::bail!(
                        "mcp-proxy spawned (pid {pid}) but did not bind to {bind} \
                         within {}ms; check that the port is free and mcp-proxy is \
                         correctly installed",
                        SPAWN_SETTLE_MS
                    );
                }

                Ok(EnsureOutcome::Spawned)
            })();
            let _ = std::fs::remove_file(&lock_path);
            result
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another caller is mid-spawn. Trust that it will publish a live
            // pid and re-read; if the pidfile still looks dead, surface that
            // as an error rather than racing again — callers can retry.
            if read_pidfile(pid_path)?.is_some() && probe_tcp(bind, LIVENESS_TCP_TIMEOUT_MS) {
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
    let (host, port) = bind
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("bind missing :port suffix: {bind}"))?;
    // Parse-don't-validate: both host and port are forwarded verbatim into the
    // child's argv (`--host <host> --port <port>`), so a value with embedded
    // spaces or flag-like content would be misread by mcp-proxy. Reject a
    // non-numeric port before it reaches the (stderr-silenced) daemon; validate
    // the host is a valid IP address so hostnames or injected flags are rejected.
    let port: u16 = port
        .parse()
        .map_err(|e| anyhow::anyhow!("bind port {port:?} is not a valid u16: {e}"))?;
    host.parse::<std::net::IpAddr>()
        .map_err(|e| anyhow::anyhow!("bind host {host:?} is not a valid IP address: {e}"))?;
    let (program, leading) = mcp_proxy_command()?;
    let mut cmd = Command::new(program);
    cmd.args(leading)
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string())
        .arg("--")
        .arg("icm")
        .arg("serve");
    configure_detached(&mut cmd);
    let child = cmd.spawn()?;
    Ok(child.id())
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
/// On all platforms stdio is redirected to the null device, which is the part
/// that fixes the pipe pollution. On Unix the child additionally joins a new
/// process group (`process_group(0)`) so foreground-group job-control signals
/// (`^C`) don't reach it; this does *not* start a new session, so a `setsid`
/// daemon would still share the controlling terminal — acceptable here because
/// `llmenv export` exits immediately after spawning, leaving the proxy
/// reparented to init. `setsid` is intentionally not used to avoid pulling in
/// `libc` (mirrors the `is_alive` rationale below).
fn configure_detached(cmd: &mut Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // 0 = make the child its own group leader.
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        // No process-group API in std on non-Unix; only the stdio redirect
        // above applies. Process-group isolation is unavailable here.
    }
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

    #[test]
    fn configure_detached_spawns_child_in_new_process_group() {
        use super::configure_detached;
        use std::process::Command;

        // `sleep` is alive long enough to inspect; we kill it before asserting.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        configure_detached(&mut cmd);
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

    /// ensure_running treats a pidfile + no listener as dead and spawns (#300).
    /// Simulates the PID-reuse scenario: the pidfile has a pid but nothing is
    /// listening on the bind address, so ensure_running must not return
    /// AlreadyRunning — it must spawn.
    #[test]
    fn ensure_running_spawns_when_pidfile_exists_but_port_is_not_bound() {
        use super::{EnsureOutcome, ensure_running, probe_tcp, write_pidfile_atomic};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        // Grab a free port then drop the listener so the port is closed.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let bind = format!("127.0.0.1:{port}");

        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");

        // Seed the pidfile with a plausible but stale PID.
        write_pidfile_atomic(&pid_path, 99_999).expect("write pidfile");

        // Confirm nothing is listening (port is free).
        assert!(!probe_tcp(&bind, 50), "port must be closed before test");

        // The spawn closure binds a listener *inside* itself so the port is
        // closed before ensure_running's fast-path probe, simulating a stale
        // pidfile with a recycled PID (#300). We store the listener in an Arc
        // so it stays alive for the post-spawn TCP probe.
        let held_listener: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
        let held2 = Arc::clone(&held_listener);
        let bind_clone = bind.clone();
        let result = ensure_running(&bind, &pid_path, move |_b| {
            let l = TcpListener::bind(&bind_clone as &str).expect("bind for spawn simulation");
            *held2.lock().expect("lock") = Some(l);
            Ok(42_u32)
        });

        assert!(result.is_ok(), "ensure_running failed: {:?}", result);
        assert_eq!(
            result.unwrap(),
            EnsureOutcome::Spawned,
            "must spawn when pidfile exists but port is not bound (PID-reuse scenario)"
        );
        // Drop the listener.
        drop(held_listener);
    }

    /// ensure_running returns AlreadyRunning when the proxy is actually bound (#300).
    #[test]
    fn ensure_running_returns_already_running_when_port_is_bound() {
        use super::{EnsureOutcome, ensure_running, write_pidfile_atomic};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let bind = format!("127.0.0.1:{port}");

        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");
        write_pidfile_atomic(&pid_path, 12_345).expect("write pidfile");

        let result = ensure_running(&bind, &pid_path, |_| {
            panic!("spawn must not be called when proxy is already running")
        });

        assert_eq!(
            result.expect("ensure_running"),
            EnsureOutcome::AlreadyRunning,
            "must return AlreadyRunning when port is bound"
        );
    }

    /// ensure_running surfaces a clear error when the spawn callback succeeds
    /// but the proxy never binds its port (#301).
    #[test]
    fn ensure_running_errors_when_spawn_succeeds_but_port_never_binds() {
        use super::ensure_running;

        // Use a closed port — spawn returns Ok but nothing ever binds.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
            // l drops here, port closed
        };
        let bind = format!("127.0.0.1:{port}");

        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("mcp-proxy.pid");

        let result = ensure_running(&bind, &pid_path, |_| Ok(99_999));

        assert!(
            result.is_err(),
            "ensure_running must error when spawn succeeds but port never binds"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("did not bind"),
            "error message should mention bind failure, got: {msg}"
        );
    }

    /// spawn_mcp_proxy rejects bind strings that have no `:port` suffix (#337).
    #[test]
    fn spawn_mcp_proxy_rejects_missing_port() {
        use super::spawn_mcp_proxy;
        let result = spawn_mcp_proxy("127.0.0.1");
        assert!(result.is_err(), "must fail without a port");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("missing :port suffix"),
            "error should mention missing port, got: {msg}"
        );
    }

    /// spawn_mcp_proxy rejects a non-numeric port so it never reaches mcp-proxy (#337).
    #[test]
    fn spawn_mcp_proxy_rejects_non_numeric_port() {
        use super::spawn_mcp_proxy;
        let result = spawn_mcp_proxy("127.0.0.1:notaport");
        assert!(result.is_err(), "must fail with a non-numeric port");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not a valid u16"),
            "error should mention invalid port, got: {msg}"
        );
    }

    /// spawn_mcp_proxy rejects a hostname host (only IP literals allowed) (#337).
    #[test]
    fn spawn_mcp_proxy_rejects_hostname_host() {
        use super::spawn_mcp_proxy;
        let result = spawn_mcp_proxy("localhost:7878");
        assert!(result.is_err(), "must reject a hostname");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not a valid IP address"),
            "error should mention invalid host, got: {msg}"
        );
    }
}
