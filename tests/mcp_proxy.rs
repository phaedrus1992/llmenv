#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Integration tests for the mcp-proxy lifecycle (#15, #300, #301, #1084–#1086).
//!
//! `ensure_running(bind, pid_path, spawn)` is the public surface. The `spawn`
//! callback is injected so tests don't actually launch `mcp-proxy`. Two
//! properties of the real contract shape the harness:
//!
//! - Liveness is decided by the bind address, so a callback simulating a
//!   successful spawn must bind a listener there (#300).
//! - The callback returns a real [`Child`], because `ensure_running` uses
//!   `Child::try_wait` to tell a running proxy from one that already exited —
//!   `kill -0` reports an unreaped child as alive (#1085). [`SpawnLog`] reaps
//!   every child it hands out when it drops.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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

/// What the injected spawn callback should hand back.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ChildKind {
    /// A process that stays alive for the rest of the test — stands in for a
    /// proxy that started successfully.
    #[default]
    Live,
    /// A process that has already exited by the time `ensure_running` inspects
    /// it — stands in for a proxy that died during startup (#1086's ImportError).
    Exited,
}

#[derive(Default)]
struct SpawnLog {
    bind_args: Mutex<Vec<String>>,
    /// Optional listener to bind per call, satisfying the post-spawn TCP probe.
    bind_listener: Mutex<Option<Arc<Mutex<Option<TcpListener>>>>>,
    child_kind: ChildKind,
    /// Pids handed out, so [`Drop`] can reap them even if a test panics.
    spawned: Mutex<Vec<u32>>,
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

    fn with_child_kind(mut self, kind: ChildKind) -> Self {
        self.child_kind = kind;
        self
    }
}

impl Drop for SpawnLog {
    fn drop(&mut self) {
        // ensure_running consumes the Child, so the pid is the only handle left.
        // Reaping here keeps `sleep` stand-ins from outliving the test run.
        for pid in self.spawned.lock().expect("lock").iter() {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Spawns a real child process to stand in for `mcp-proxy`.
///
/// `Live` uses `sleep`, which never binds anything — the listener (if any) is
/// bound by the callback itself, mirroring how the real proxy's socket is
/// independent of the handle `ensure_running` holds.
fn stand_in_child(kind: ChildKind) -> Child {
    let mut child = match kind {
        ChildKind::Live => Command::new("sleep").arg("30").spawn(),
        ChildKind::Exited => Command::new("false").spawn(),
    }
    .expect("spawn stand-in child");
    if kind == ChildKind::Exited {
        // Make the exit observable before ensure_running calls try_wait, without
        // leaving a zombie that kill -0 would misreport as alive (#1085).
        let _ = child.wait();
    }
    child
}

fn spawner(log: Arc<SpawnLog>) -> impl Fn(&str) -> anyhow::Result<Child> {
    move |bind: &str| {
        log.bind_args.lock().expect("lock").push(bind.to_owned());
        // If a listener holder is configured, bind the port to satisfy the
        // post-spawn TCP probe in ensure_running.
        if let Some(holder) = log.bind_listener.lock().expect("lock").as_ref() {
            let l = TcpListener::bind(bind).expect("bind for spawn");
            *holder.lock().expect("lock") = Some(l);
        }
        let child = stand_in_child(log.child_kind);
        log.spawned.lock().expect("lock").push(child.id());
        Ok(child)
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
    // The proxy is "alive" when something is accepting TCP connections on the
    // bind address. The pidfile is not part of that judgement (#300, #1085).
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
// Post-spawn liveness check (#301, #1086)
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_errors_when_the_proxy_dies_before_binding() {
    // The realistic startup failure: mcp-proxy launches, fails to import, and
    // exits without ever opening its socket (#1086).
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    // No listener holder — spawn will not bind the port.
    let log = Arc::new(SpawnLog::default().with_child_kind(ChildKind::Exited));

    let result = ensure_running(&bind, &pid_path, spawner(log.clone()));

    let msg = result.expect_err("must error when the proxy dies before binding");
    let msg = msg.to_string();
    assert!(
        msg.contains("before binding"),
        "error must say the proxy died before binding, got: {msg}"
    );
    assert!(
        msg.contains("mcp-proxy.log"),
        "error must point at the stderr log so the cause is findable, got: {msg}"
    );
    assert!(
        !msg.contains("correctly installed") && !msg.contains("port is free"),
        "error must not restate the guesses that misled in #1086, got: {msg}"
    );
    assert!(
        !pid_path.exists(),
        "a proxy that never bound must not leave a pidfile behind"
    );
}

#[test]
fn ensure_running_does_not_record_a_pid_that_is_not_the_listener() {
    // The #1085 trap: our child dies immediately because an orphaned proxy
    // already holds the port. The post-spawn probe succeeds — but it succeeds
    // against the orphan, so the dead child's pid must not be published.
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    // The callback binds the port (standing in for the orphan) and hands back a
    // child that has already exited (standing in for the one that lost the race).
    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(
        SpawnLog::default()
            .with_listener_holder(Arc::clone(&held))
            .with_child_kind(ChildKind::Exited),
    );

    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(
        outcome,
        EnsureOutcome::AlreadyRunning,
        "a proxy we did not start is AlreadyRunning, not Spawned — this is what \
         keeps the listen_host warning from firing on a run that started nothing"
    );
    assert!(
        !pid_path.exists(),
        "the dead child's pid must not be written to the pidfile"
    );

    drop(held);
}

#[test]
fn ensure_running_records_the_live_child_pid() {
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let (_, bind) = free_port();

    let held: Arc<Mutex<Option<TcpListener>>> = Arc::new(Mutex::new(None));
    let log = Arc::new(SpawnLog::default().with_listener_holder(Arc::clone(&held)));

    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");
    assert_eq!(outcome, EnsureOutcome::Spawned);

    let written: u32 = std::fs::read_to_string(&pid_path)
        .expect("read pidfile")
        .trim()
        .parse()
        .expect("parse pid");
    let spawned = log.spawned.lock().expect("lock").clone();
    assert_eq!(
        spawned.as_slice(),
        &[written],
        "the pidfile must name the child we spawned"
    );
    assert!(is_alive(written), "the recorded pid must be a live process");

    drop(held);
}

// ---------------------------------------------------------------------------
// Pidfile reconciliation (#1085)
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_clears_a_dead_pid_when_the_port_is_serving() {
    // The state #1084 + #1085 leave behind: a live proxy plus a pidfile naming a
    // process that no longer exists. The pidfile must not survive as a permanent
    // lie — the old fast path read any non-empty pidfile as proof of life.
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let bind = listener.local_addr().expect("addr").to_string();

    let dead = 4_000_004_u32;
    assert!(!is_alive(dead), "fixture pid must be dead");
    std::fs::write(&pid_path, dead.to_string()).expect("write stale pidfile");

    let log = Arc::new(SpawnLog::default());
    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    assert_eq!(log.calls(), 0, "must not spawn while the port is served");
    assert!(
        !pid_path.exists(),
        "a pidfile naming a dead process must be cleared"
    );

    drop(listener);
}

#[test]
fn ensure_running_keeps_a_live_pid_when_the_port_is_serving() {
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let bind = listener.local_addr().expect("addr").to_string();

    let live = std::process::id();
    std::fs::write(&pid_path, live.to_string()).expect("write pidfile");

    let log = Arc::new(SpawnLog::default());
    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    let kept: u32 = std::fs::read_to_string(&pid_path)
        .expect("read pidfile")
        .trim()
        .parse()
        .expect("parse pid");
    assert_eq!(kept, live, "a live pid must be left alone");

    drop(listener);
}

#[test]
fn ensure_running_clears_an_unparseable_pidfile_instead_of_failing() {
    // A garbage pidfile used to abort the whole export with a parse error, even
    // though the proxy was up and the pidfile is not what proves liveness (#1085).
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let bind = listener.local_addr().expect("addr").to_string();

    std::fs::write(&pid_path, "garbage").expect("write pidfile");

    let log = Arc::new(SpawnLog::default());
    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    assert_eq!(log.calls(), 0);
    assert!(!pid_path.exists(), "garbage pidfile must be cleared");

    drop(listener);
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

// ---------------------------------------------------------------------------
// #1085: the port is the source of truth, not the pidfile
// ---------------------------------------------------------------------------

#[test]
fn ensure_running_is_already_running_when_listening_and_pidfile_absent() {
    // A live proxy whose pidfile went missing (e.g. deleted by #1084's bogus
    // bind-failure cleanup) must still read as running: the port is the source
    // of truth. Spawning here is what produced the orphan/dead-pid state.
    let _guard = port_guard();
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let bind = format!("127.0.0.1:{port}");

    assert!(!pid_path.exists(), "pidfile must be absent for this case");

    let log = Arc::new(SpawnLog::default());
    let outcome = ensure_running(&bind, &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    assert_eq!(
        log.calls(),
        0,
        "must not spawn while something is already serving the bind address"
    );

    drop(listener);
}
