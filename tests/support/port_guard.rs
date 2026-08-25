// Shared by `src/mcp/proxy.rs`'s `#[cfg(test)]` module and `tests/mcp_proxy.rs`'s
// integration binary via `include!` — the two can't import from each other (an
// integration test only sees the crate's public API, and there is none for
// test-only serialization), so this file is pasted in at each include site
// instead of duplicated by hand (#1494; mirrors #1481/#1488 on the 4.x line).

/// Fixed advisory-lock address serializing every test in this codebase that
/// allocates an ephemeral TCP port (#1494). `cargo test`/`cargo nextest` can
/// run the crate's `--lib` binary and its `tests/mcp_proxy.rs` integration
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
/// squatter that has nothing to do with test contention.
const NETWORK_TEST_LOCK_ADDR: &str = "127.0.0.1:20990";

/// Acquires [`NETWORK_TEST_LOCK_ADDR`], retrying until it succeeds.
///
/// Only retries on `AddrInUse` — genuine contention with a sibling test
/// binary holding the lock. Any other bind error (permission denied, address
/// unavailable, fd exhaustion) is not contention and won't resolve itself by
/// waiting, so it panics immediately with the real `io::Error` rather than
/// burning the full retry budget and then blaming "another process" for a
/// cause that was never contention (pre-pr-review finding, #1494).
fn port_guard() -> std::net::TcpListener {
    let mut last_err = None;
    for _ in 0..250 {
        match std::net::TcpListener::bind(NETWORK_TEST_LOCK_ADDR) {
            Ok(l) => return l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => last_err = Some(e),
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
