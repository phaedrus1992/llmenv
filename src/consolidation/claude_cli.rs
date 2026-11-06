//! The isolated `claude -p` child that consolidation runs (#2355).

use super::CHILD_GUARD_ENV;

/// The isolated `claude -p` call. The child inherits `CLAUDE_CONFIG_DIR`, so
/// without these flags it runs every llmenv hook and MCP server, and its own
/// `SessionEnd` hook starts consolidation again (#2355).
///
/// The user's settings still load, because they can hold the auth (`env`,
/// `apiKeyHelper`, a Bedrock or Vertex provider). `--bare` and
/// `--setting-sources ""` both drop that auth, and `--bare` also drops the
/// OAuth login this backend exists for. `disableAllHooks` stops the hooks,
/// and the default output style keeps the reply in the bullet format that
/// [`super::bullets::parse_bullets`] reads.
pub(super) fn claude_command(program: &std::path::Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args([
        "-p",
        "--settings",
        r#"{"disableAllHooks":true,"outputStyle":"default"}"#,
        // No --mcp-config is given, so no MCP server starts.
        "--strict-mcp-config",
        "--tools",
        "",
        "--disable-slash-commands",
        "--no-session-persistence",
    ])
    .env(CHILD_GUARD_ENV, "1")
    // The prompt calls no tools, so waiting for MCP servers to connect only adds start-up time.
    // Claude Code 2.1.274 and later read this; older versions ignore it.
    .env("CLAUDE_CODE_MCP_STARTUP_WAIT_MS", "0");
    cmd
}
