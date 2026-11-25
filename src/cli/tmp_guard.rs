//! `llmenv tmp-guard`: deny a tool call that writes under `/tmp` (#4045).
//!
//! The auto-registered `PreToolUse` hook runs this command for the shell tools and the file
//! tools. It reads the call from stdin and prints a deny envelope, or prints nothing to let the
//! call run. The checks are in `hook_run::tmp_guard`.

use std::io::Read as _;

/// Run `llmenv tmp-guard`. The command exits 0 in every case. A deny is the envelope on stdout.
pub(super) fn run(_engine: &str) {
    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv tmp-guard: failed to read stdin, so the call was not checked: {e}");
        return;
    }
    match decide(&stdin_buf) {
        Ok(Some(envelope)) => println!("{envelope}"),
        Ok(None) => {}
        Err(e) => eprintln!(
            "llmenv tmp-guard: the hook payload is not JSON, so the call was not checked: {e}"
        ),
    }
}

/// The deny envelope for the call in `payload`, or `None` when the call may run.
///
/// # Errors
/// Fails when `payload` is not JSON.
fn decide(payload: &str) -> Result<Option<String>, serde_json::Error> {
    let payload: serde_json::Value = serde_json::from_str(payload)?;
    Ok(
        crate::hook_run::tmp_guard::deny_reason(&payload).map(|reason| {
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            })
            .to_string()
        }),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::decide;

    #[test]
    fn a_tmp_write_gives_a_deny_envelope_that_claude_code_reads() {
        let payload = r#"{"tool_name":"Write","tool_input":{"file_path":"/tmp/x.md"}}"#;
        let out: serde_json::Value =
            serde_json::from_str(&decide(payload).unwrap().unwrap()).unwrap();
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
        assert_eq!(decide(payload).unwrap(), None);
    }

    #[test]
    fn a_payload_that_is_not_json_is_an_error() {
        assert!(decide("not json").is_err());
    }
}
