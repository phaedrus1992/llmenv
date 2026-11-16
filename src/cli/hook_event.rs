//! The hook event name that the engine sends to `llmenv config-context`.

/// The hook event that the engine sent on stdin. Empty stdin is the SessionStart hook run by
/// hand, so it gets `SessionStart`. Any other input must be a JSON object with a string
/// `hook_event_name`, because a made-up event name would mislead the engine (#3752).
pub(super) fn hook_event_name_from(stdin: &str) -> anyhow::Result<String> {
    if stdin.trim().is_empty() {
        return Ok("SessionStart".to_owned());
    }
    let shown: String = stdin.chars().take(80).collect();
    let value: serde_json::Value = serde_json::from_str(stdin).map_err(|e| {
        anyhow::anyhow!(
            "config-context: stdin is not JSON ({e}): {shown:?}. Fix: the engine must send a \
             hook payload with a string \"hook_event_name\"."
        )
    })?;
    value["hook_event_name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "config-context: stdin has no string \"hook_event_name\": {shown:?}. Fix: the \
                 engine must send a hook payload with a string \"hook_event_name\"."
            )
        })
}

#[cfg(test)]
mod tests {
    use super::hook_event_name_from;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn blank_input_is_session_start(blank in "[ \\t\\r\\n]{0,8}") {
            prop_assert_eq!(hook_event_name_from(&blank).unwrap(), "SessionStart");
        }

        #[test]
        fn a_string_event_name_is_returned_exactly(name in "[A-Za-z]{1,20}", id in "[a-z0-9]{0,10}") {
            let payload = serde_json::json!({"hook_event_name": name, "session_id": id}).to_string();
            prop_assert_eq!(hook_event_name_from(&payload).unwrap(), name);
        }

        #[test]
        fn a_non_string_event_name_is_an_error(n in 0u32..100) {
            let payload = serde_json::json!({"hook_event_name": n}).to_string();
            prop_assert!(hook_event_name_from(&payload).is_err());
        }

        #[test]
        fn text_that_is_not_json_never_panics_and_is_an_error(text in "[a-zA-Z ]{1,40}") {
            prop_assume!(!text.trim().is_empty());
            prop_assert!(hook_event_name_from(&text).is_err());
        }
    }
}
