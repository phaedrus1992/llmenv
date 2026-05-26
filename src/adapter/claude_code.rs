use std::path::Path;

use serde_json::json;

use super::AgentAdapter;
use crate::config::Icm;
use crate::merge::MergedManifest;

/// Name used to register the ICM MCP server in Claude Code's `mcp.json`. Also
/// the substitution value for `{{ICM_MCP}}` placeholders in bundle hook
/// templates so bundle hooks can reference the MCP by name without knowing
/// it ahead of time.
pub const ICM_MCP_NAME: &str = "icm";

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

        // Copy all files from the manifest. JSON hook templates get
        // `{{ICM_MCP}}` substituted so bundle hooks can reference the MCP
        // server by name without hard-coding it.
        for (rel, abs) in &manifest.files {
            let dest = out.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if is_hook_json(rel) {
                let raw = std::fs::read_to_string(abs)?;
                let rendered = raw.replace("{{ICM_MCP}}", ICM_MCP_NAME);
                std::fs::write(&dest, rendered)?;
            } else {
                std::fs::copy(abs, &dest)?;
            }
        }

        // Validate that skills are properly structured with SKILL.md frontmatter
        validate_skills(out)?;

        // Generate settings.json from hook/permission bundles
        generate_settings_json(out)?;

        // Emit mcp.json when the manifest carries ICM config
        if let Some(icm) = &manifest.icm {
            write_mcp_json(out, icm, manifest.icm_is_server)?;
        }

        Ok(())
    }
}

/// True if `rel` is a JSON file under the bundle's `hooks/` subtree —
/// these files are template-rendered rather than byte-copied so bundle hooks
/// can reference the ICM MCP via `{{ICM_MCP}}`.
fn is_hook_json(rel: &Path) -> bool {
    rel.starts_with("hooks") && rel.extension().is_some_and(|e| e == "json")
}

/// Writes `mcp.json` registering the ICM MCP server.
///
/// When `is_server` is true this host is the ICM server: register a local
/// stdio entry that spawns `icm mcp-server` (the hook ensures `mcp-proxy` is
/// running separately so other clients on the network can reach it).
///
/// When false: register an HTTP client pointing at `icm.client_url`.
fn write_mcp_json(out: &Path, icm: &Icm, is_server: bool) -> anyhow::Result<()> {
    let entry = if is_server {
        json!({
            "command": "icm",
            "args": ["mcp-server"],
        })
    } else {
        json!({
            "url": icm.client_url,
        })
    };
    let doc = json!({
        "mcpServers": {
            ICM_MCP_NAME: entry,
        },
    });
    let path = out.join("mcp.json");
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
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

        if let Some(frontmatter_end) = content.find("\n---\n").or_else(|| {
            if content.ends_with("---") {
                Some(content.len() - 3)
            } else {
                None
            }
        }) {
            let frontmatter_str = &content[3..frontmatter_end];
            match serde_yaml::from_str::<serde_yaml::Mapping>(frontmatter_str) {
                Ok(mapping) => {
                    let has_name = mapping.get("name").is_some();
                    let has_description = mapping.get("description").is_some();

                    if !has_name || !has_description {
                        return Err(anyhow::anyhow!(
                            "Skill {} SKILL.md missing required frontmatter fields (name and description)",
                            path.display()
                        ));
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Skill {} SKILL.md has invalid YAML frontmatter: {}",
                        path.display(),
                        e
                    ));
                }
            }
        } else {
            return Err(anyhow::anyhow!(
                "Skill {} SKILL.md missing YAML frontmatter (must start with --- and end with ---)",
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
