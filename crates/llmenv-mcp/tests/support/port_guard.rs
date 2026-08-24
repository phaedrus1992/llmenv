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
const NETWORK_TEST_LOCK_ADDR: &str = "127.0.0.1:47990";

/// Acquires [`NETWORK_TEST_LOCK_ADDR`], retrying until it succeeds.
fn port_guard() -> std::net::TcpListener {
    for _ in 0..250 {
        if let Ok(l) = std::net::TcpListener::bind(NETWORK_TEST_LOCK_ADDR) {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "could not acquire the network-test advisory lock on {NETWORK_TEST_LOCK_ADDR} after \
         5s; is another process bound to it?"
    );
}
