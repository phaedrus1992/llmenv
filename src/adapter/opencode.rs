use std::path::{Path, PathBuf};

#[expect(
    unused_imports,
    reason = "used in Task 2 when materialize is implemented"
)]
use serde_json::json;

use super::AgentAdapter;
use crate::merge::MergedManifest;

/// Adapter for opencode: writes `AGENTS.md` and `opencode.json` into the
/// cache dir and exports `OPENCODE_CONFIG_DIR` so opencode discovers them.
///
/// Skills use the claude-compatible `SKILL.md` format opencode reads natively.
/// Hooks are bridged via a generated `plugin/llmenv.js` shim (§3).
#[derive(Debug, Default, Clone, Copy)]
pub struct OpencodeAdapter;

#[expect(dead_code, reason = "used in Task 2 when materialize is implemented")]
const OPENCODE_JSON_FILE: &str = "opencode.json";

/// opencode supports exactly these hook events via its JS plugin API.
/// See spec §3 for the event mapping.
const SUPPORTED_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

impl AgentAdapter for OpencodeAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn binary_name(&self) -> &'static str {
        "opencode"
    }

    fn supports_plugins(&self) -> bool {
        true
    }

    fn supports_lsp(&self) -> bool {
        true
    }

    fn supported_hook_events(&self) -> &'static [&'static str] {
        SUPPORTED_HOOK_EVENTS
    }

    fn env_vars(
        &self,
        cache_dir: &Path,
        _state_dir: &Path,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        Ok(vec![("OPENCODE_CONFIG_DIR".into(), dir.to_owned())])
    }

    fn materialize(&self, _manifest: &MergedManifest, _out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        anyhow::bail!("not yet implemented")
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event_name,
                "additionalContext": format!("[ICM MEMORY CONTEXT (auto-injected)]\n{text}"),
            }
        })
        .to_string()
    }
}
