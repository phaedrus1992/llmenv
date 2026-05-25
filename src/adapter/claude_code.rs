use std::path::Path;

use super::AgentAdapter;
use crate::merge::MergedManifest;

/// Adapter for Claude Code: writes `CLAUDE.md` (from `agents_md`) and copies
/// all merged files into `out`. Sets `CLAUDE_CONFIG_DIR` so Claude Code uses
/// `out` as its config root.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn env_vars(&self, cache_dir: &Path) -> Vec<(String, String)> {
        vec![(
            "CLAUDE_CONFIG_DIR".into(),
            cache_dir.to_string_lossy().into_owned(),
        )]
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(out)?;
        std::fs::write(out.join("CLAUDE.md"), &manifest.agents_md)?;
        for (rel, abs) in &manifest.files {
            let dest = out.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(abs, &dest)?;
        }
        Ok(())
    }
}
