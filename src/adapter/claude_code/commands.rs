//! The llmenv command lines that the hooks of the Claude Code adapter run.

use crate::adapter::hook_program::hook_command;

/// Command the auto-emitted SessionStart hook runs to inject source config paths
/// into agent context (#289). Outputs `hookSpecificOutput.additionalContext` JSON
/// so the agent always knows where to edit config rather than touching the cache.
pub(super) fn config_context_command() -> anyhow::Result<String> {
    hook_command("config-context --engine claude_code")
}

/// Command the auto-emitted PreToolUse hook runs to guard against writes to the
/// managed cache directory (#289). Reads the Write/Edit/MultiEdit tool call from
/// stdin and prints a redirection hint if the target is a cache path. Exits 0
/// (fail-soft) so the write still proceeds; the hint keeps agents oriented.
pub(super) fn config_guard_command() -> anyhow::Result<String> {
    hook_command("config-guard --engine claude_code")
}

/// Command the auto-emitted throttle hooks run. Throttle hooks fire on
/// PreToolUse and UserPromptSubmit to poll the usage backend and sleep a
/// capped adaptive delay to avoid rate limits.
pub(super) fn throttle_command() -> anyhow::Result<String> {
    hook_command("throttle")
}

/// Prefix of the auto-emitted lifecycle/session-log hook commands. The full
/// command is `HOOK_RUN_COMMAND <neutral_event>`, e.g.
/// `llmenv hook-run --engine claude_code session_start`. Dispatches ICM memory
/// wake-up/store (#197/#228) and, per `session_log` config, the session-log
/// file/transcript sinks (#382). Always fail-soft (exit 0).
pub(super) fn hook_run_command() -> anyhow::Result<String> {
    hook_command("hook-run --engine claude_code")
}
