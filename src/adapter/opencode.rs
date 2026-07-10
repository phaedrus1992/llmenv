use std::path::{Path, PathBuf};

use super::AgentAdapter;
use crate::merge::MergedManifest;

/// Adapter for opencode: writes `AGENTS.md` and `opencode.json` into the
/// cache dir and exports `OPENCODE_CONFIG_DIR` so opencode discovers them.
///
/// Skills use the claude-compatible `SKILL.md` format opencode reads natively.
/// Hooks are bridged via a generated `plugin/llmenv.js` shim (§3).
#[derive(Debug, Default, Clone, Copy)]
pub struct OpencodeAdapter;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::rules::RuleFile;

    const VALID_FRONTMATTER: &str = "---\nname: x\ndescription: y\n---\nbody\n";

    #[test]
    fn materialize_empty_manifest_writes_agents_md_and_json() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        assert!(
            owned.contains(&PathBuf::from("AGENTS.md")),
            "owned must include AGENTS.md, got: {owned:?}"
        );
        assert!(
            owned.contains(&PathBuf::from(OPENCODE_JSON_FILE)),
            "owned must include opencode.json, got: {owned:?}"
        );
    }

    #[test]
    fn materialize_agents_md_content_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.agents_md = "# Test Rules\n\nSome content here.".to_string();
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert_eq!(content, "# Test Rules\n\nSome content here.");
    }

    #[test]
    fn materialize_rules_copied_and_listed_in_instructions() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            rules: vec![
                RuleFile {
                    bundle: "test".into(),
                    rel: PathBuf::from("rules/security.md"),
                    frontmatter: None,
                    body: "# Security\n\ncontent".into(),
                    raw: "# Security\n\ncontent".into(),
                },
                RuleFile {
                    bundle: "test".into(),
                    rel: PathBuf::from("rules/style.md"),
                    frontmatter: None,
                    body: "# Style\n\ncontent".into(),
                    raw: "# Style\n\ncontent".into(),
                },
            ],
            ..Default::default()
        };
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        assert!(
            owned.contains(&PathBuf::from("rules/security.md")),
            "owned must include rules/security.md, got: {owned:?}"
        );
        assert!(
            tmp.path().join("rules/style.md").exists(),
            "rules/style.md must exist"
        );
        let json_raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&json_raw).unwrap();
        let instr = doc["instructions"].as_array().unwrap();
        assert!(
            instr.contains(&serde_json::json!("rules/security.md")),
            "instructions must include rules/security.md"
        );
        assert!(
            instr.contains(&serde_json::json!("rules/style.md")),
            "instructions must include rules/style.md"
        );
    }

    #[test]
    fn materialize_first_class_skills() {
        let out = tempfile::tempdir().unwrap();
        let skill_src = tempfile::tempdir().unwrap();
        std::fs::create_dir(skill_src.path().join("subdir")).unwrap();
        std::fs::write(skill_src.path().join("SKILL.md"), VALID_FRONTMATTER).unwrap();
        std::fs::write(
            skill_src.path().join("subdir/helper.sh"),
            "#!/bin/sh\necho hi\n",
        )
        .unwrap();

        let mut manifest = MergedManifest::default();
        manifest.capabilities.skills = vec![crate::config::SkillSource {
            name: "my-oc-skill".into(),
            path: skill_src.path().to_str().unwrap().into(),
            when: Vec::new(),
        }];
        OpencodeAdapter.materialize(&manifest, out.path()).unwrap();

        assert!(out.path().join("skills/my-oc-skill/SKILL.md").exists());
        assert!(
            out.path()
                .join("skills/my-oc-skill/subdir/helper.sh")
                .exists()
        );
    }

    #[test]
    fn materialize_plugin_projected_skills() {
        let out = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("skills/my-plugin-skill")).unwrap();
        std::fs::write(
            plugin_dir.path().join("skills/my-plugin-skill/SKILL.md"),
            VALID_FRONTMATTER,
        )
        .unwrap();

        let mut manifest = MergedManifest::default();
        manifest.plugins = vec![crate::plugins::resolve::ResolvedPlugin {
            marketplace: "test".into(),
            plugin: "my-plugin".into(),
            collection: String::new(),
            install_path: Some(plugin_dir.path().to_str().unwrap().into()),
            git_commit_sha: None,
        }];
        manifest.marketplaces = vec![crate::plugins::resolve::ResolvedMarketplace {
            name: "test".into(),
            source: String::new(),
            install_location: None,
            head: None,
        }];
        OpencodeAdapter.materialize(&manifest, out.path()).unwrap();

        assert!(
            out.path().join("skills/my-plugin-skill/SKILL.md").exists(),
            "plugin-projected skill must exist"
        );
    }
}

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

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        super::skills::create_dir_owner_only(out)?;

        let mut owned: Vec<PathBuf> = Vec::new();

        // 1. AGENTS.md
        super::skills::reject_hardcoded_config_path(&manifest.agents_md, "AGENTS.md")?;
        crate::paths::write_owner_only(&out.join("AGENTS.md"), manifest.agents_md.as_bytes())?;
        owned.push(PathBuf::from("AGENTS.md"));

        // 2. rules/*.md — written verbatim; paths collected for instructions[]
        let mut instructions: Vec<String> = Vec::new();
        for r in &manifest.rules {
            if crate::paths::is_unsafe_join_target(r.rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in rules file: {}", r.rel.display());
            }
            super::skills::reject_hardcoded_config_path(&r.raw, &r.rel.to_string_lossy())?;
            let dest = out.join(&r.rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::paths::write_owner_only(&dest, r.raw.as_bytes())?;
            instructions.push(r.rel.to_string_lossy().into_owned());
            owned.push(r.rel.clone());
        }

        // 3. First-class skills (declared via `capabilities.skills`).
        let skill_owned =
            crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;
        owned.extend(skill_owned);

        // 4. Plugin-projected skills (skills dir inside a plugin payload).
        for plugin in &manifest.plugins {
            let payload = super::resolve_plugin_payload(plugin, &manifest.marketplaces)?;
            let paths = crate::adapter::skills::project_plugin_skills(&payload, out)?;
            owned.extend(paths);
        }

        // 5. Validate skills (frontmatter + hardcoded-path scan).
        crate::adapter::skills::validate_skills(out)?;

        // 6. Build opencode.json with what we have so far
        let mut doc = serde_json::Map::new();
        doc.insert(
            "$schema".into(),
            serde_json::json!("https://opencode.ai/config.json"),
        );
        if !instructions.is_empty() {
            doc.insert("instructions".into(), serde_json::json!(instructions));
        }

        // 4. Write opencode.json
        let json_bytes = serde_json::to_vec_pretty(&doc)?;
        let out_path = out.join(OPENCODE_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;
        owned.push(PathBuf::from(OPENCODE_JSON_FILE));

        Ok(owned)
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
