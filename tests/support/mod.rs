#![expect(clippy::expect_used, reason = "test scaffolding")]

//! Shared test-isolation helper (#1089).
//!
//! `LLMENV_CONFIG_DIR` alone is not isolation: `state_dir()` keys off
//! `LLMENV_STATE_DIR`/`HOME` (`crates/llmenv-paths/src/lib.rs`), and the
//! mcp-proxy pidfile/log paths key off `XDG_STATE_HOME`/`HOME`
//! (`src/mcp/proxy.rs`) — separate knobs a test can leave pointed at the
//! developer's real directories by only overriding the config dir. This
//! helper overrides every one of them to the same tempdir so a test can't
//! get isolation half-right.

use std::path::Path;

use assert_cmd::Command;

/// Build a `Command` for the `llmenv` binary with every directory knob the
/// binary consults (`LLMENV_CONFIG_DIR`, `LLMENV_STATE_DIR`,
/// `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `HOME`) pointed at `dir`, so nothing
/// it runs can read or write outside the test's own tempdir.
pub fn isolated_llmenv_cmd(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").expect("find llmenv binary");
    cmd.env("LLMENV_CONFIG_DIR", dir)
        .env("LLMENV_STATE_DIR", dir)
        .env("XDG_STATE_HOME", dir)
        .env("XDG_CACHE_HOME", dir)
        .env("HOME", dir);
    cmd
}
