//! Warn-only `PreToolUse` advisory for Bash `cd` (#976).
//!
//! The archived-transcript corpus shows 77 occurrences of "Shell cwd was
//! reset to <path>" — the harness resets the working directory after every
//! Bash call that `cd`s, so any *following* command that assumed the new cwd
//! runs in the wrong place. The base bundle's own prose guidance ("prefer
//! absolute paths over `cd`") wasn't stopping this on its own; mechanizing
//! the reminder into a non-blocking advisory is the fix.
//!
//! Stateless, unlike `read_once`/`repeat_detect`: it only inspects the
//! current call's `tool_input.command`, no per-session tracking needed.

use crate::config::CdGuard;

const ADVISORY: &str = "note: this command changes the working directory with `cd` — \
Claude Code resets the cwd after every Bash call, so a following command that assumes \
the new directory will run in the wrong place. Prefer an absolute path instead.";

/// Handle a `PreToolUse` event for the cd-guard feature. Returns the
/// advisory text, or an empty string when the guard doesn't apply (disabled,
/// not a Bash call, or no `cd` detected).
pub fn handle_pre_tool_use(stdin_payload: &serde_json::Value, cfg: &CdGuard) -> String {
    if !cfg.enabled {
        return String::new();
    }
    let Some("Bash") = stdin_payload.get("tool_name").and_then(|v| v.as_str()) else {
        return String::new();
    };
    let Some(command) = stdin_payload
        .get("tool_input")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
    else {
        // Claude Code's Bash tool schema guarantees `command` is present as a
        // string — this should never fire in practice. Log it rather than
        // silently no-op, in case the harness's payload shape ever drifts.
        tracing::debug!("tool_input.command missing or not a string for Bash PreToolUse payload");
        return String::new();
    };
    if command_uses_cd(command) {
        ADVISORY.to_string()
    } else {
        String::new()
    }
}

/// Whether `command` contains a `cd` invocation as any top-level segment
/// (split on `&&`, `||`, `;`, `|`, or newline) — the shape that triggers the
/// cwd-reset behavior this guard warns about.
///
/// A lightweight heuristic, not a shell parser: `cd` inside a string
/// literal, subshell, or quoted argument may be misdetected (false positive,
/// acceptable for a non-blocking advisory) or missed if nested deeper (false
/// negative, also acceptable — this only needs to catch the common case
/// prose guidance wasn't stopping).
fn command_uses_cd(command: &str) -> bool {
    command
        .split(['\n', ';', '|'])
        .flat_map(|seg| seg.split("&&"))
        .flat_map(|seg| seg.split("||"))
        .any(|segment| segment.split_whitespace().next() == Some("cd"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bash_payload(command: &str) -> serde_json::Value {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command },
        })
    }

    #[test]
    fn detects_bare_cd() {
        assert!(command_uses_cd("cd /tmp"));
    }

    #[test]
    fn detects_leading_cd_in_compound_and() {
        assert!(command_uses_cd("cd /tmp && ls"));
    }

    #[test]
    fn detects_trailing_cd_in_compound_and() {
        assert!(command_uses_cd("ls && cd /tmp"));
    }

    #[test]
    fn detects_cd_after_semicolon() {
        assert!(command_uses_cd("ls; cd /tmp"));
    }

    #[test]
    fn detects_cd_after_or() {
        assert!(command_uses_cd("false || cd /tmp"));
    }

    #[test]
    fn detects_cd_across_newlines() {
        assert!(command_uses_cd("ls\ncd /tmp"));
    }

    #[test]
    fn ignores_command_without_cd() {
        assert!(!command_uses_cd("ls -la /tmp"));
    }

    #[test]
    fn ignores_cd_as_a_substring_of_another_word() {
        assert!(!command_uses_cd("mkdir abcd && ls"));
    }

    #[test]
    fn ignores_cdpath_env_assignment() {
        assert!(!command_uses_cd("CDPATH=/tmp ls"));
    }

    #[test]
    fn handle_pre_tool_use_returns_advisory_for_cd_command() {
        let text = handle_pre_tool_use(&bash_payload("cd /tmp && ls"), &CdGuard { enabled: true });
        assert!(!text.is_empty());
        assert!(text.contains("cd"));
    }

    #[test]
    fn handle_pre_tool_use_empty_when_disabled() {
        let text = handle_pre_tool_use(&bash_payload("cd /tmp"), &CdGuard { enabled: false });
        assert!(text.is_empty());
    }

    #[test]
    fn handle_pre_tool_use_empty_for_non_bash_tool() {
        let payload = serde_json::json!({
            "tool_name": "Read",
            "tool_input": { "file_path": "/tmp/x" },
        });
        let text = handle_pre_tool_use(&payload, &CdGuard { enabled: true });
        assert!(text.is_empty());
    }

    #[test]
    fn handle_pre_tool_use_empty_when_no_cd() {
        let text = handle_pre_tool_use(&bash_payload("ls -la"), &CdGuard { enabled: true });
        assert!(text.is_empty());
    }

    // #1207: a Bash payload missing tool_input.command should never happen in
    // practice (Claude Code's schema guarantees it), but if the harness's
    // payload shape ever drifts, cd_guard would otherwise stop firing with no
    // trace in the logs.
    #[test]
    fn handle_pre_tool_use_logs_debug_when_bash_command_missing() {
        let payload = serde_json::json!({ "tool_name": "Bash", "tool_input": {} });
        let logs = crate::test_log_capture::capture_debug_logs(|| {
            let text = handle_pre_tool_use(&payload, &CdGuard { enabled: true });
            assert!(text.is_empty());
        });
        assert!(
            logs.contains("tool_input.command missing"),
            "expected a debug log when tool_input.command is absent"
        );
    }

    proptest! {
        // Arbitrary text never panics, regardless of shell-metacharacter content.
        #[test]
        fn command_uses_cd_never_panics(command in ".{0,200}") {
            let _ = command_uses_cd(&command);
        }
    }
}
