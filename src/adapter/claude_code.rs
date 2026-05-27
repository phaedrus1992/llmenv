use std::path::Path;

use anyhow::Context;
use serde_json::json;

use super::AgentAdapter;
use crate::mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, ResolvedMcp};
use crate::merge::MergedManifest;

/// Substitution value for `{{ICM_MCP}}` placeholders in bundle hook templates,
/// so bundle hooks can reference the memory MCP server by name without knowing
/// it ahead of time. Tracks the memory backend's registration name.
const ICM_MCP_NAME: &str = MEMORY_MCP_NAME;

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

        // Claude Code has a native rules-directory convention, so write each
        // `rules/*.md` file verbatim (frontmatter preserved) into `<out>/rules/`.
        // Adapters that lack this convention should instead use
        // `merge::agents_md::concat_with_rules` to inline the bodies.
        for r in &manifest.rules {
            let dest = out.join(&r.rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &r.raw)?;
        }

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
        generate_settings_json(out, manifest)?;

        // Emit mcp.json when the manifest carries any resolved MCP servers.
        if !manifest.mcps.is_empty() {
            write_mcp_json(out, &manifest.mcps)?;
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

/// Writes `mcp.json` registering every resolved MCP server under the
/// `mcpServers` key. Stdio entries carry `command`/`args`/`env`; remote entries
/// carry `url`. Entries are keyed by server name.
fn write_mcp_json(out: &Path, mcps: &[ResolvedMcp]) -> anyhow::Result<()> {
    let mut servers = serde_json::Map::new();
    for m in mcps {
        let entry = match &m.kind {
            ResolvedKind::Stdio { command, args, env } => {
                let mut obj = json!({ "command": command, "args": args });
                if !env.is_empty() {
                    obj["env"] = json!(env);
                }
                obj
            }
            ResolvedKind::Remote { url, .. } => json!({ "url": url }),
        };
        servers.insert(m.name.clone(), entry);
    }
    let doc = json!({ "mcpServers": servers });
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

/// Generates settings.json from hook/permission contributions in the manifest.
///
/// Issue #90: Wires hook fragments into Claude Code's `hooks.{Event}[].hooks` array.
/// Each event groups its hooks as `{ matcher: "...", hooks: [{ command, tool, type }] }`.
///
/// Issue #91: Merges top-level `native.claude_code` keys into settings.json with
/// hard-error on collision (native key conflicts with capability-generated key).
///
/// Issue #85: Prerequisite for SessionStart hook (wiring complete, hash comparison
/// logic deferred to runtime hook script).
fn generate_settings_json(out: &Path, manifest: &MergedManifest) -> anyhow::Result<()> {
    let mut settings = serde_json::Map::new();

    // #90: Transform hooks: Vec<Hook> into { EventName: [{ matcher, hooks: [...] }] }
    // Design: https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md
    let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for hook in &manifest.capabilities.hooks {
        let handler = json!({
            "command": hook.handler.command,
            "tool": hook.handler.tool,
            "type": match hook.handler.kind {
                crate::config::HookHandlerKind::Command => "command",
                crate::config::HookHandlerKind::McpTool => "mcp_tool",
            },
        });

        let mut hook_entry = serde_json::Map::new();
        if let Some(matcher) = &hook.matcher {
            hook_entry.insert("matcher".into(), json!(matcher));
        }
        hook_entry.insert("hooks".into(), json!([handler]));

        hooks_by_event
            .entry(hook.event.clone())
            .or_default()
            .push(serde_json::Value::Object(hook_entry));
    }

    let mut hooks_obj = serde_json::Map::new();
    for (event, entries) in hooks_by_event {
        hooks_obj.insert(event, json!(entries));
    }
    settings.insert("hooks".into(), serde_json::Value::Object(hooks_obj));

    // #91: Merge native permission rules with collision detection
    if settings.contains_key("permissions") {
        return Err(anyhow::anyhow!(
            "Collision: native.claude_code permissions conflict with capability-generated permissions key. \
             Hard-error policy (O3): cannot merge. Review bundle.yaml and config.yaml for overlapping native/capability contributions."
        ));
    }

    if let Some(native_rules) = manifest.capabilities.permissions.native.get("claude_code") {
        let mut perm_obj = serde_json::Map::new();
        perm_obj.insert(
            "native".into(),
            json!({
                "allow": native_rules.allow,
                "ask": native_rules.ask,
                "deny": native_rules.deny,
            }),
        );
        settings.insert("permissions".into(), serde_json::Value::Object(perm_obj));
    }

    let settings_value = serde_json::Value::Object(settings);
    let settings_path = out.join("settings.json");
    let json_str = serde_json::to_string_pretty(&settings_value)?;

    std::fs::write(&settings_path, &json_str).with_context(|| {
        format!(
            "Failed to write settings.json at {}",
            settings_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&settings_path, perms).with_context(|| {
            format!(
                "Failed to set permissions on settings.json at {}",
                settings_path.display()
            )
        })?;
    }

    Ok(())
}
