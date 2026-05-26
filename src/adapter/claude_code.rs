use std::path::Path;

use serde_json::json;

use super::AgentAdapter;
use crate::merge::MergedManifest;

/// Adapter for Claude Code: writes `CLAUDE.md` (from `agents_md`) and copies
/// all merged files into `out`. Sets `CLAUDE_CONFIG_DIR` so Claude Code uses
/// `out` as its config root.
///
/// Skills are structured as directories with a `SKILL.md` file containing YAML
/// frontmatter (at minimum `name` and `description`).
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn env_vars(&self, cache_dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        Ok(vec![("CLAUDE_CONFIG_DIR".into(), dir.to_owned())])
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(out)?;
        std::fs::write(out.join("CLAUDE.md"), &manifest.agents_md)?;

        // Copy all files from the manifest
        for (rel, abs) in &manifest.files {
            let dest = out.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(abs, &dest)?;
        }

        // Validate that skills are properly structured with SKILL.md frontmatter
        validate_skills(out)?;

        // Generate settings.json from hook/permission bundles
        generate_settings_json(out)?;

        Ok(())
    }
}

/// Validates that all skills in the materialized directory have SKILL.md with required frontmatter.
fn validate_skills(out: &Path) -> anyhow::Result<()> {
    let skills_dir = out.join("skills");
    if !skills_dir.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&skills_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Skip non-directories
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            return Err(anyhow::anyhow!(
                "Skill directory {} missing SKILL.md",
                path.display()
            ));
        }

        let content = std::fs::read_to_string(&skill_md)?;
        // Check that it has YAML frontmatter markers
        if !content.starts_with("---") {
            return Err(anyhow::anyhow!(
                "Skill {} SKILL.md missing YAML frontmatter (must start with ---)",
                path.display()
            ));
        }

        // Check for required fields in frontmatter (simple check: lines starting with "name:" or "description:")
        let has_name = content.lines().any(|l| l.trim().starts_with("name:"));
        let has_description = content.lines().any(|l| l.trim().starts_with("description:"));

        if !has_name || !has_description {
            return Err(anyhow::anyhow!(
                "Skill {} SKILL.md missing required frontmatter fields (name and description)",
                path.display()
            ));
        }
    }

    Ok(())
}

/// Generates settings.json from hook/permission contributions in the materialized directory.
///
/// Currently creates a minimal settings.json placeholder. Full hook/permission merging
/// (issue #34) is deferred to a follow-up PR.
fn generate_settings_json(out: &Path) -> anyhow::Result<()> {
    let settings = json!({
        "hooks": [],
        "permissions": [],
        "mcp": []
    });

    let settings_path = out.join("settings.json");
    let json_str = serde_json::to_string_pretty(&settings)?;
    std::fs::write(settings_path, json_str)?;

    Ok(())
}
