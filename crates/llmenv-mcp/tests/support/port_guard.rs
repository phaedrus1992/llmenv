// Shared by `src/proxy.rs`'s `#[cfg(test)]` module and this crate's
// `tests/mcp_proxy.rs` integration binary via `include!` — the two can't
// import from each other (an integration test only sees this crate's public
// API, and there is none for test-only serialization), so this file is
// pasted in at each include site instead of duplicated by hand (#1486).

/// Fixed advisory-lock address serializing every test in this crate that
/// allocates an ephemeral TCP port (#1481). `cargo test`/`cargo nextest` can
/// run this crate's `--lib` binary and its `tests/mcp_proxy.rs` integration
/// binary as separate, concurrent processes — `cargo nextest` runs every
/// individual test as its own process — so an in-process `Mutex` cannot stop
/// one binary's test from reusing a port the other just freed. Binding a
/// fixed, otherwise unused port as an advisory lock works across the process
/// boundary: only one bind can hold it at a time, anywhere.
///
/// Deliberately below both platforms' default ephemeral ranges (Linux
/// `net.ipv4.ip_local_port_range` 32768-60999, macOS 49152-65535) — a port
/// inside either range can be handed to an unrelated process by the kernel,
/// which would make this lock spin out its retry budget and panic on a
/// squatter that has nothing to do with test contention (#1497).
const NETWORK_TEST_LOCK_ADDR: &str = "127.0.0.1:20990";

/// Whether a bind failure is contention with a sibling test binary holding
/// the lock (worth retrying) versus a different failure — permission denied,
/// address unavailable, fd exhaustion — that won't resolve itself by
/// waiting (#1497).
fn is_retryable_bind_error(kind: std::io::ErrorKind) -> bool {
    kind == std::io::ErrorKind::AddrInUse
}

/// Acquires [`NETWORK_TEST_LOCK_ADDR`], retrying until it succeeds.
///
/// Only retries on genuine contention (see [`is_retryable_bind_error`]); any
/// other bind error panics immediately with the real `io::Error` rather than
/// burning the full retry budget and then blaming "another process" for a
/// cause that was never contention (#1497).
fn port_guard() -> std::net::TcpListener {
    let mut last_err = None;
    for _ in 0..250 {
        match std::net::TcpListener::bind(NETWORK_TEST_LOCK_ADDR) {
            Ok(l) => return l,
            Err(e) if is_retryable_bind_error(e.kind()) => last_err = Some(e),
            Err(e) => panic!(
                "unexpected error binding the network-test advisory lock on \
                 {NETWORK_TEST_LOCK_ADDR}: {e} (kind: {:?}) — this is not address \
                 contention, retrying would not help",
                e.kind()
            ),
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "could not acquire the network-test advisory lock on {NETWORK_TEST_LOCK_ADDR} after \
         5s (last error: {}); is another process bound to it?",
        last_err.map_or_else(|| "unknown".to_string(), |e| e.to_string())
    );
}

#[test]
fn network_test_lock_addr_is_outside_default_ephemeral_ranges() {
    let port: u16 = NETWORK_TEST_LOCK_ADDR
        .rsplit(':')
        .next()
        .expect("address has a port")
        .parse()
        .expect("port is numeric");
    assert!(
        port < 32768,
        "port {port} falls inside Linux's default ephemeral range (32768-60999)"
    );
    assert!(
        port < 49152,
        "port {port} falls inside macOS's default ephemeral range (49152-65535)"
    );
}

#[test]
fn only_retries_on_addr_in_use() {
    assert!(is_retryable_bind_error(std::io::ErrorKind::AddrInUse));
    assert!(!is_retryable_bind_error(
        std::io::ErrorKind::PermissionDenied
    ));
}
