#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Integration tests for the mcp-proxy lifecycle (#15, #300, #301).
//!
//! `ensure_running(bind, pid_path, spawn)` is the public surface. The `spawn`
//! callback is injected so tests don't actually launch `mcp-proxy`. Because
//! ensure_running now validates liveness via a TCP probe (#300), spawn callbacks
//! that simulate a successful spawn must bind a listener on the bind address so
//! the post-spawn probe succeeds.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use llmenv::mcp::proxy::{EnsureOutcome, ensure_running, is_alive, probe_tcp};
use tempfile::tempdir;

/// Serializes every test that allocates an ephemeral port. cargo runs the tests
/// in a binary in parallel, and [`free_port`] releases its port before the test
/// asserts the port is closed (or before the spawn callback rebinds it). A
/// sibling test binding `127.0.0.1:0` can grab that just-freed port and flake
/// the victim. Holding this lock across the whole body of every port-touching
/// test removes the intra-binary race. A poisoned lock (a prior test panicked
/// mid-body) is recovered rather than propagated — the guarded data is `()`.
fn port_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Allocates an ephemeral TCP port by binding then dropping the listener, and
/// confirms the released port is actually closed before returning it.
///
/// On macOS the kernel reuses just-freed ephemeral ports aggressively, so a
/// bare bind-then-drop can hand back a port that still probes as open (a prior
/// listener draining, or the same number reassigned). Tests that assert
/// "port must be closed before test" then flake. We re-pick until a probe
/// confirms the port refuses connections. Callers must hold [`port_guard`] so
/// no sibling test reopens the port between this check and use.
fn free_port() -> (u16, String) {
    for _ in 0..50 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let port = l.local_addr().expect("addr").port();
        let bind = format!("127.0.0.1:{port}");
        drop(l);
        if !probe_tcp(&bind, 50) {
            return (port, bind);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("could not obtain a confirmed-closed ephemeral port after retries");
}

#[derive(Default)]
struct SpawnLog {
    bind_args: Mutex<Vec<String>>,
    next_pid: AtomicU32,
    /// Optional listener to bind per call, satisfying the post-spawn TCP probe.
    bind_listener: Mutex<Option<Arc<Mutex<Option<TcpListener>>>>>,
}

impl SpawnLog {
    fn calls(&self) -> usize {
        self.bind_args.lock().expect("lock").len()
    }

    /// Configure the log to bind a listener on each spawn call.
    fn with_listener_holder(self, holder: Arc<Mutex<Option<TcpListener>>>) -> Self {
        *self.bind_listener.lock().expect("lock") = Some(holder);
        self
    }
}

fn spawner(log: Arc<SpawnLog>) -> impl Fn(&str) -> anyhow::Result<u32> {
    move |bind: &str| {
        log.bind_args.lock().expect("lock").push(bind.to_owned());
        // If a listener holder is configured, bind the port to satisfy the
        // post-spawn TCP probe in ensure_running.
        if let Some(holder) = log.bind_listener.lock().expect("lock").as_ref() {
            let l = TcpListener::bind(bind).expect("bind for spawn");
            *holder.lock().expect("lock") = Some(l);
        }
        let pid = log.next_pid.fetch_add(1, Ordering::SeqCst) + 4_000_000;
        Ok(pid)
    }
}

// ---------------------------------------------------------------------------
// Basic spawn path
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_spawns_when_no_pidfile() {
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    // Keep the listener alive so the post-spawn probe succeeds.
    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::Spawned);
    assert_eq!(log.calls(), 1, "spawn must be called exactly once");
    assert!(pid_path.exists(), "pidfile must be written after spawn");
}

#[test]
fn ensure_running_passes_bind_to_spawner() {
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    let calls = log.bind_args.lock().expect("lock");
    assert_eq!(calls.as_slice(), &[bind]);
}

// ---------------------------------------------------------------------------
// Liveness: existing proxy
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_no_op_when_proxy_is_listening() {
    // The proxy is "alive" when it has a pidfile AND is accepting TCP connections.
    // This is the new contract (#300 — TCP probe replaces kill-0).
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    // Bind a listener to simulate an already-running proxy.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let bind = format!("127.0.0.1:{port}");

    std::fs::write(&pid_path, "12345").expect("write pidfile");

    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    assert_eq!(
        log.calls(),
        0,
        "spawn must not be called when proxy is listening"
    );

    drop(listener);
}

#[test]
fn ensure_running_respawns_when_pidfile_exists_but_port_closed() {
    // Pidfile exists but port is not bound — simulates a stale pidfile or PID
    // reuse: a different process holds the old PID but the proxy is gone (#300).
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    // Write a stale pidfile.
    std::fs::write(&pid_path, "4000001").expect("write stale pidfile");

    // Port is closed (nothing listening) — probe must return false.
    assert!(!probe_tcp(&bind, 50), "port must be closed before test");

    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::Spawned);
    assert_eq!(
        log.calls(),
        1,
        "spawn must be called when port is not bound"
    );

    let contents = std::fs::read_to_string(&pid_path).expect("read pid");
    let parsed: u32 = contents.trim().parse().expect("parse pid");
    assert_ne!(
        parsed, 4_000_001,
        "pidfile must be overwritten with new pid"
    );

    drop(held);
}

// ---------------------------------------------------------------------------
// Post-spawn liveness check (#301)
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_errors_when_spawn_succeeds_but_port_never_binds() {
    // Spawn callback returns Ok but never binds the port — simulates a crashed
    // or misconfigured mcp-proxy that exits before opening its socket (#301).
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    // No listener holder — spawn will not bind the port.
    let log = Arc::new(SpawnLog::default());

    let result = ensure_running(&bind, &pid_path, spawner(log.clone()));

    assert!(
        result.is_err(),
        "must error when spawn succeeds but port never binds"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("did not bind"),
        "error must mention bind failure, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Locking / concurrency guards
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_errors_when_lock_is_held_and_port_closed() {
    // Simulate a peer holding the lockfile mid-spawn: pidfile is stale and
    // port is not bound. ensure_running must NOT spawn and must surface an error.
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let lock_path: PathBuf = tmp.path().join("mcp-proxy.pid.lock");
    let (_, bind) = free_port();
    std::fs::write(&lock_path, "").expect("write lockfile");

    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    let result = ensure_running(&bind, &pid_path, spawner(log.clone()));

    assert!(
        result.is_err(),
        "should error when peer holds lock and port is closed"
    );
    assert_eq!(log.calls(), 0, "must not spawn while lock is held");
}

#[test]
fn ensure_running_accepts_peer_published_pid_when_listening() {
    // Peer holds the lock AND the proxy is now listening. We must observe
    // AlreadyRunning rather than racing them (#300).
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let lock_path: PathBuf = tmp.path().join("mcp-proxy.pid.lock");

    // Bind a listener to simulate the peer's proxy already running.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let bind = format!("127.0.0.1:{port}");

    std::fs::write(&lock_path, "").expect("write lockfile");
    std::fs::write(&pid_path, "12345").expect("write pid");

    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    assert_eq!(log.calls(), 0);

    drop(listener);
}

// ---------------------------------------------------------------------------
// is_alive / probe_tcp primitives
// ---------------------------------------------------------------------------

#[test]
fn is_alive_returns_false_for_almost_certainly_dead_pid() {
    // is_alive still available for non-proxy callers; must not panic.
    assert!(!is_alive(4_000_002));
}

#[test]
fn is_alive_returns_true_for_self() {
    let my_pid = std::process::id();
    assert!(is_alive(my_pid));
}

#[test]
fn probe_tcp_returns_false_for_invalid_address() {
    // An unparseable bind address can never connect — probe must return false.
    assert!(
        !probe_tcp("not-a-valid-address", 100),
        "probe_tcp must return false for unparseable address"
    );
    // Port 0 is never bound by a real server; connect_timeout to it is refused.
    assert!(
        !probe_tcp("127.0.0.1:0", 100),
        "probe_tcp must return false for port 0"
    );
}

#[test]
fn probe_tcp_returns_true_for_open_port() {
    let _guard = port_guard();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let bind = format!("127.0.0.1:{port}");
    assert!(
        probe_tcp(&bind, 200),
        "probe_tcp must return true when listener is bound"
    );
    drop(listener);
}
