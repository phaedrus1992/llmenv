//! `llmenv tmp-guard`: deny a tool call that writes under `/tmp` (#4045).
//!
//! The auto-registered `PreToolUse` hook runs this command for the shell tools and the file
//! tools. It reads the call from stdin and prints a deny envelope, or prints nothing to let the
//! call run. The checks are in `hook_run::tmp_guard`.
//!
//! A payload that cannot be read is denied. The stderr of an exit-0 hook does not reach the
//! agent, so an allow would hide that the call was not checked.

use std::io::Read as _;

use crate::hook_run::tmp_guard::{deny_reason, unreadable_reason};

/// Run `llmenv tmp-guard`. The command exits 0 in every case. A deny is the envelope on stdout.
pub(super) fn run(_engine: &str) {
    let mut stdin_buf = String::new();
    let read = std::io::stdin().read_to_string(&mut stdin_buf);
    let reason = match read {
        Ok(_) => decide(&stdin_buf),
        Err(e) => Some(unreadable_reason(&format!(
            "the hook payload cannot be read from stdin ({e})"
        ))),
    };
    if let Some(reason) = reason {
        tracing::error!("llmenv tmp-guard denied a call: {reason}");
        println!("{}", envelope(&reason));
    }
}

/// The reason to deny the call in `payload`, or `None` when the call may run. A payload that is
/// not JSON is denied.
fn decide(payload: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(payload) => deny_reason(&payload),
        Err(e) => Some(unreadable_reason(&format!(
            "the hook payload is not JSON ({e})"
        ))),
    }
}

/// The `PreToolUse` deny envelope that Claude Code reads.
fn envelope(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{decide, envelope};

    #[test]
    fn a_tmp_write_gives_a_deny_envelope_that_claude_code_reads() {
        let payload = r#"{"tool_name":"Write","tool_input":{"file_path":"/tmp/x.md"}}"#;
        let out: serde_json::Value =
            serde_json::from_str(&envelope(&decide(payload).unwrap())).unwrap();
        let hook = &out["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "PreToolUse");
        assert_eq!(hook["permissionDecision"], "deny");
        assert!(
            hook["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("Use \"$TMPDIR\" for scratch files. /tmp is not cleaned up.")
        );
    }

    #[test]
    fn an_allowed_call_prints_nothing() {
        let payload = r#"{"tool_name":"Bash","tool_input":{"command":"ls /tmp"}}"#;
        assert_eq!(decide(payload), None);
    }

    #[test]
    fn a_payload_that_is_not_json_is_denied_with_the_cause() {
        let reason = decide("not json").unwrap();
        assert!(reason.contains("the hook payload is not JSON"), "{reason}");
        assert!(reason.contains("cannot be checked"), "{reason}");
    }
}
