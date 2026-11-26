//! Per-engine rules for hook payloads and decisions.
//!
//! Each engine sends or reads hook data differently, so these rules stay out of `mod.rs`.

use serde_json::Value;

/// Applies the payload rules for `engine`. Only Crush needs one: its tool names differ.
pub(crate) fn neutralize_for_engine(engine: &str, payload: Option<&mut Value>) {
    if let Some(payload) = payload.filter(|_| engine == "crush") {
        neutralize_crush_tool_name(payload);
    }
}

/// Renames `tool_name` in a Crush hook payload to its neutral name, such as `bash` to `Bash`.
///
/// The guards match Claude Code's names. Crush's input keys for the tools the guards read
/// (`command`, `file_path`) already match Claude Code's, so only the name changes. A tool
/// with no neutral name keeps its Crush name, so no guard matches it.
pub(super) fn neutralize_crush_tool_name(payload: &mut Value) {
    let Some(name) = payload.get("tool_name").and_then(Value::as_str) else {
        return;
    };
    if let Some(neutral) = crate::adapter::tools::neutral_for_crush(name) {
        payload["tool_name"] = Value::String(neutral.to_string());
    }
}

/// Whether `engine`'s hook bridge decides a tool call was blocked from the
/// process exit code rather than the JSON envelope on stdout.
///
/// opencode's generated plugin shim blocks on `code === 2`, and it also reads the deny envelope
/// on stdout (`readResult` in `plugin.js`). Exit 2 is the signal that a gate with `failClosed`
/// always reads, so a deny from `hook-run` uses it there.
/// Claude Code honours the envelope itself, and exit 2 would only duplicate a decision it has
/// already made.
///
/// `engine` is [`crate::adapter::AgentAdapter::name`]'s hyphenated cache-dir
/// form, the same value `should_check_stale` takes — see the warning there.
pub(super) fn blocks_by_exit_code(engine: &str) -> bool {
    engine == "opencode"
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{neutralize_crush_tool_name, neutralize_for_engine};

    #[test]
    fn crush_bash_becomes_neutral_bash_and_keeps_its_input() {
        let mut payload = json!({ "tool_name": "bash", "tool_input": { "command": "ls" } });
        neutralize_crush_tool_name(&mut payload);
        assert_eq!(payload["tool_name"], "Bash");
        assert_eq!(payload["tool_input"]["command"], "ls");
    }

    #[test]
    fn crush_view_becomes_neutral_read() {
        let mut payload = json!({ "tool_name": "view", "tool_input": { "file_path": "/a" } });
        neutralize_crush_tool_name(&mut payload);
        assert_eq!(payload["tool_name"], "Read");
    }

    #[test]
    fn tool_without_neutral_name_keeps_its_crush_name() {
        let mut payload = json!({ "tool_name": "agentic_fetch" });
        neutralize_crush_tool_name(&mut payload);
        assert_eq!(payload["tool_name"], "agentic_fetch");
    }

    #[test]
    fn engine_gate_renames_crush_payloads_only() {
        let mut crush = json!({ "tool_name": "bash" });
        neutralize_for_engine("crush", Some(&mut crush));
        assert_eq!(crush["tool_name"], "Bash");

        let mut other = json!({ "tool_name": "bash" });
        neutralize_for_engine("claude-code", Some(&mut other));
        assert_eq!(other["tool_name"], "bash");
    }

    #[test]
    fn engine_gate_accepts_a_missing_payload() {
        neutralize_for_engine("crush", None);
    }

    #[test]
    fn payload_without_tool_name_is_left_alone() {
        let mut payload = json!({ "session_id": "s" });
        neutralize_crush_tool_name(&mut payload);
        assert_eq!(payload, json!({ "session_id": "s" }));
    }
}
