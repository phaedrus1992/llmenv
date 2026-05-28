#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests for #15: mcp-proxy lifecycle for ICM server host.
//!
//! `ensure_running(bind, pid_path, spawn)` is the public surface. The `spawn`
//! callback is injected so tests don't actually launch `mcp-proxy` — we just
//! verify the pidfile logic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use llmenv::mcp::proxy::{EnsureOutcome, ensure_running, is_alive};
use tempfile::tempdir;

#[derive(Default)]
struct SpawnLog {
    bind_args: Mutex<Vec<String>>,
    next_pid: AtomicU32,
}

impl SpawnLog {
    fn calls(&self) -> usize {
        self.bind_args.lock().expect("lock").len()
    }
}

fn spawner(log: Arc<SpawnLog>) -> impl Fn(&str) -> anyhow::Result<u32> {
    move |bind: &str| {
        log.bind_args.lock().expect("lock").push(bind.to_owned());
        // Allocate a fresh, definitely-not-running pid every call — we use a
        // very high number to avoid colliding with a real process.
        let pid = log.next_pid.fetch_add(1, Ordering::SeqCst) + 4_000_000;
        Ok(pid)
    }
}

#[test]
fn ensure_running_spawns_when_no_pidfile() {
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let log = Arc::new(SpawnLog::default());

    let outcome =
        ensure_running("127.0.0.1:8765", &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::Spawned);
    assert_eq!(log.calls(), 1, "spawn must be called exactly once");
    assert!(pid_path.exists(), "pidfile must be written after spawn");
}

#[test]
fn ensure_running_no_op_when_pid_alive() {
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    // Write our own pid — definitely alive.
    let my_pid = std::process::id();
    std::fs::write(&pid_path, my_pid.to_string()).expect("write pidfile");

    let log = Arc::new(SpawnLog::default());
    let outcome =
        ensure_running("127.0.0.1:8765", &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::AlreadyRunning);
    assert_eq!(log.calls(), 0, "spawn must not be called when pid is alive");
}

#[test]
fn ensure_running_respawns_when_pid_dead() {
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");

    // Write a very high pid that's almost certainly not in use.
    std::fs::write(&pid_path, "4000001").expect("write stale pidfile");

    let log = Arc::new(SpawnLog::default());
    let outcome =
        ensure_running("127.0.0.1:8765", &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, EnsureOutcome::Spawned);
    assert_eq!(log.calls(), 1, "spawn must be called when pidfile is stale");

    let contents = std::fs::read_to_string(&pid_path).expect("read pid");
    let parsed: u32 = contents.trim().parse().expect("parse pid");
    assert_ne!(
        parsed, 4_000_001,
        "pidfile must be overwritten with new pid"
    );
}

#[test]
fn ensure_running_passes_bind_to_spawner() {
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let log = Arc::new(SpawnLog::default());

    ensure_running("0.0.0.0:9999", &pid_path, spawner(log.clone())).expect("ensure_running");

    let calls = log.bind_args.lock().expect("lock");
    assert_eq!(calls.as_slice(), &["0.0.0.0:9999".to_owned()]);
}

#[test]
fn is_alive_returns_false_for_almost_certainly_dead_pid() {
    // Pick a pid that's almost certainly not in use. is_alive must not panic
    // and must return false for a process we cannot signal.
    assert!(!is_alive(4_000_002));
}

#[test]
fn is_alive_returns_true_for_self() {
    let my_pid = std::process::id();
    assert!(is_alive(my_pid));
}

#[test]
fn ensure_running_errors_when_lock_is_held_and_pid_dead() {
    // Simulate a peer holding the lockfile mid-spawn: pidfile is stale (or
    // empty) but the .lock sibling exists. ensure_running must NOT spawn —
    // that's the bug the lock prevents — and must surface an error instead.
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let lock_path: PathBuf = tmp.path().join("mcp-proxy.pid.lock");
    std::fs::write(&lock_path, "").expect("write lockfile");

    let log = Arc::new(SpawnLog::default());
    let result = ensure_running("127.0.0.1:8765", &pid_path, spawner(log.clone()));

    assert!(result.is_err(), "should error when peer holds lock");
    assert_eq!(log.calls(), 0, "must not spawn while lock is held");
}

#[test]
fn ensure_running_accepts_peer_published_pid() {
    // Peer holds the lock AND has published a live pid. We must observe
    // AlreadyRunning rather than racing them.
    let tmp = tempdir().expect("tempdir");
    let pid_path: PathBuf = tmp.path().join("mcp-proxy.pid");
    let lock_path: PathBuf = tmp.path().join("mcp-proxy.pid.lock");
    std::fs::write(&lock_path, "").expect("write lockfile");
    std::fs::write(&pid_path, std::process::id().to_string()).expect("write pid");

    let log = Arc::new(SpawnLog::default());
    let outcome =
        ensure_running("127.0.0.1:8765", &pid_path, spawner(log.clone())).expect("ensure_running");

    assert_eq!(outcome, llmenv::mcp::proxy::EnsureOutcome::AlreadyRunning);
    assert_eq!(log.calls(), 0);
}
