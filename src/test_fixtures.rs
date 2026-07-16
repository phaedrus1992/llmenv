//! Shared golden hook-payload fixtures for tests (#839).
//!
//! Fixture JSON lives under `tests/fixtures/hook_payloads/` and mirrors the
//! real snake_case wire shape Claude Code sends on `PreToolUse`/`PostToolUse`
//! hooks — see `tests/fixtures/hook_payloads/README.md`. A prior bug (#724)
//! shipped because unit tests used a hand-typed camelCase payload instead of
//! this real shape, so the tests passed against fake data while the real
//! integration silently no-op'd. Fixtures here must be sourced from real
//! Claude Code hook invocations, not hand-typed guesses.

#![expect(clippy::panic, reason = "a broken fixture should fail loudly")]

use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hook_payloads")
        .join(name)
}

/// Read a hook-payload fixture's raw JSON text, e.g. for feeding directly
/// into a stdin-shaped parser under test.
pub(crate) fn load_hook_payload_raw(name: &str) -> String {
    let path = fixture_path(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
}

/// Read and parse a hook-payload fixture into a [`serde_json::Value`].
pub(crate) fn load_hook_payload(name: &str) -> serde_json::Value {
    let raw = load_hook_payload_raw(name);
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse fixture {name}: {e}"))
}
