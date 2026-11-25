//! The `PreToolUse` guards that every Claude Code session registers, with no config gating.

use serde_json::json;

use super::commands::{config_guard_command, hook_run_command, tmp_guard_command};
use super::tools_matcher;

/// Add the guard entries to the `PreToolUse` list of `hooks_by_event`.
///
/// # Errors
/// Fails when llmenv cannot name its own program for a hook command.
pub(super) fn register(
    hooks_by_event: &mut std::collections::BTreeMap<String, Vec<serde_json::Value>>,
) -> anyhow::Result<()> {
    let pre = hooks_by_event.entry("PreToolUse".to_string()).or_default();

    // #289: warn the agent when it tries to write inside the managed cache dir.
    // Anchored regex so only exact tool names match, not substrings like BatchEdit.
    // Exits 0 (fail-soft, never blocks the write).
    pre.push(json!({
        "matcher": "^(Write|Edit|MultiEdit)$",
        "hooks": [{ "type": "command", "command": config_guard_command()? }],
    }));

    // #4045: deny a write under /tmp, because nothing cleans it. A separate command, so no other
    // hook layer sees these calls.
    pre.push(json!({
        "matcher": tools_matcher(&crate::hook_run::tmp_guard::guarded_tools()),
        "hooks": [{ "type": "command", "command": tmp_guard_command()? }],
    }));

    // #318: read-once file dedup hook — warn or deny repeated file reads.
    // The hook-run handler in `run_inner` checks `features.read_once.enabled` and returns empty
    // (pass-through) when disabled, so the regex match is the only cost when the feature is off.
    pre.push(json!({
        "matcher": "^Read$",
        "hooks": [{ "type": "command", "command": format!("{} pre_tool_use", hook_run_command()?) }],
    }));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    // #4045: the guard loads in every session, so an empty manifest carries it too.
    #[test]
    fn tmp_guard_is_registered_in_every_session_for_the_shell_and_file_tools() {
        let tmp = tempfile::tempdir().unwrap();
        super::super::generate_settings_json(tmp.path(), &crate::merge::MergedManifest::default())
            .unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join("settings.json")).unwrap())
                .unwrap();
        let entry = settings["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["hooks"][0]["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("tmp-guard --engine claude_code"))
            })
            .expect("a PreToolUse entry runs tmp-guard");
        assert_eq!(
            entry["matcher"],
            "^(Bash|Monitor|Write|Edit|MultiEdit|NotebookEdit)$"
        );
    }
}
