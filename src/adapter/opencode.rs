use std::path::{Path, PathBuf};

use super::AgentAdapter;
use crate::mcp::resolve::ResolvedKind;
use crate::merge::MergedManifest;
use crate::util::dedup;

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

/// Template for `plugin/llmenv.js` — a self-contained ES module bridging
/// opencode's JS plugin API to llmenv's hook-run subprocess calls.
/// `${HOOK_TABLE}` is replaced at render time with a JSON array of
/// `{ event, opencode, commands: [{command, timeout}] }` entries.
const SHIM_TEMPLATE: &str = r#"// llmenv hook bridge for opencode — auto-generated, do not edit.
const HOOK_TABLE = ${HOOK_TABLE};

let sessionContext = null;

export default {
  id: "llmenv-hooks",
  name: "llmenv",
  dispose() {
    runHooks("SessionEnd", null);
  },
  async event(input) {
    const event = input.event;
    if (event.event === "session.created") {
      sessionContext = await runHooks("SessionStart", null);
    } else if (event.event === "session.idle") {
      runHooks("Stop", null);
    } else if (event.event === "session.deleted") {
      runHooks("SessionEnd", null);
    }
  },
  async "chat.message"(input, output) {
    const ctx = await runHooks("UserPromptSubmit", null);
    if (ctx && output.message && output.message.content && Array.isArray(output.message.content)) {
      output.message.content.push({ type: "text", text: ctx });
    }
    if (sessionContext && output.message && output.message.content && Array.isArray(output.message.content)) {
      output.message.content.push({ type: "text", text: `Additional context: ${sessionContext}` });
      sessionContext = null;
    }
  },
  async "tool.execute.before"(input, output) {
    const ctx = await runHooks("PreToolUse", {
      tool_name: input.tool,
      tool_input: JSON.stringify(input.output?.args || {}),
    });
    if (ctx === "__LLMENV_BLOCK__") {
      throw new Error("Blocked by llmenv hook");
    }
  },
  async "tool.execute.after"(input, output) {
    runHooks("PostToolUse", {
      tool_name: input.tool,
      tool_input: JSON.stringify(input.args || {}),
    });
  },
};

async function runHooks(event, extra) {
  const entries = HOOK_TABLE.filter(e => e.event === event);
  let collected = "";
  for (const entry of entries) {
    for (const hk of entry.commands) {
      try {
        const result = await spawnHook(event, hk.command, hk.timeout, extra);
        if (result.blocked) {
          if (event === "PreToolUse") return "__LLMENV_BLOCK__";
          continue;
        }
        if (result.stdout) {
          try {
            const parsed = JSON.parse(result.stdout);
            if (parsed?.hookSpecificOutput?.additionalContext) {
              collected += parsed.hookSpecificOutput.additionalContext + "\n";
            } else {
              collected += result.stdout + "\n";
            }
          } catch {
            collected += result.stdout + "\n";
          }
        }
      } catch (e) {
        console.error(`llmenv hook ${event} failed:`, e);
      }
    }
  }
  return collected || null;
}

async function spawnHook(event, command, timeoutMs, extra) {
  const payload = {
    hook_event_name: event,
    session_id: process.env.OPENCODE_SESSION_ID || "",
    cwd: process.cwd(),
    ...(extra || {}),
  };
  const { spawn } = await import("node:child_process");
  return new Promise((resolve) => {
    const child = spawn("sh", ["-c", command], {
      stdio: ["pipe", "pipe", "pipe"],
      timeout: timeoutMs || 30000,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => { stdout += d.toString(); });
    child.stderr.on("data", (d) => { stderr += d.toString(); });
    child.on("close", (code) => {
      resolve({
        blocked: code === 2 && event === "PreToolUse",
        stdout: stdout.trim() || null,
        stderr: stderr.trim() || null,
      });
    });
    child.stdin.write(JSON.stringify(payload));
    child.stdin.end();
  });
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::resolve::ResolvedMcp;
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

    #[test]
    fn materialize_mcp_local_server_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(ResolvedMcp {
            name: "local-srv".into(),
            kind: super::ResolvedKind::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@anthropic-ai/mcp-server".into()],
                env: std::collections::BTreeMap::from([("FOO".into(), "bar".into())]),
            },
            headers: std::collections::BTreeMap::new(),
            timeout: Some(10_000),
            disabled_tools: vec![],
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let srv = &doc["mcp"]["local-srv"];
        assert_eq!(srv["type"], serde_json::json!("local"));
        let cmd = srv["command"].as_array().unwrap();
        assert_eq!(cmd[0], serde_json::json!("npx"));
        assert_eq!(cmd[1], serde_json::json!("-y"));
        assert_eq!(srv["environment"]["FOO"], serde_json::json!("bar"));
        assert_eq!(srv["timeout"], serde_json::json!(10_000));
    }

    #[test]
    fn materialize_mcp_remote_server_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(ResolvedMcp {
            name: "remote-srv".into(),
            kind: super::ResolvedKind::Remote {
                url: "http://localhost:3000/mcp".into(),
                transport: crate::config::McpTransport::Http,
            },
            headers: std::collections::BTreeMap::from([(
                "Authorization".into(),
                "Bearer xyz".into(),
            )]),
            timeout: Some(5000),
            disabled_tools: vec![],
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let srv = &doc["mcp"]["remote-srv"];
        assert_eq!(srv["type"], serde_json::json!("remote"));
        assert_eq!(srv["url"], serde_json::json!("http://localhost:3000/mcp"));
        assert_eq!(
            srv["headers"]["Authorization"],
            serde_json::json!("Bearer xyz")
        );
    }

    #[test]
    fn materialize_mcp_optional_fields_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(ResolvedMcp {
            name: "minimal".into(),
            kind: super::ResolvedKind::Remote {
                url: "http://example.com".into(),
                transport: crate::config::McpTransport::Http,
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let srv = &doc["mcp"]["minimal"];
        assert!(srv.get("headers").is_none());
        assert!(srv.get("timeout").is_none());
        assert!(srv.get("cwd").is_none());
        assert!(
            srv.get("disabled_tools").is_none(),
            "opencode has no disabled_tools field"
        );
    }

    #[test]
    fn materialize_lsp_server_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.lsp.push(llmenv_config::LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            args: vec!["--quiet".into()],
            filetypes: vec!["rust".into()],
            env: std::collections::BTreeMap::from([("RUST_LOG".into(), "info".into())]),
            timeout: Some(60),
            ..Default::default()
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let srv = &doc["lsp"]["rust-analyzer"];
        let cmd = srv["command"].as_array().unwrap();
        assert_eq!(cmd[0], serde_json::json!("rust-analyzer"));
        assert_eq!(cmd[1], serde_json::json!("--quiet"));
        assert_eq!(srv["env"]["RUST_LOG"], serde_json::json!("info"));
        let exts = srv["extensions"].as_array().unwrap();
        assert!(exts.contains(&serde_json::json!("rust")));
    }

    #[test]
    fn materialize_lsp_with_init_options() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.lsp.push(llmenv_config::LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            init_options: Some(serde_yaml::from_str("checkOnSave: true").unwrap()),
            ..Default::default()
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["lsp"]["rust-analyzer"]["initialization"]["checkOnSave"],
            serde_json::json!(true)
        );
        assert!(
            doc["lsp"]["rust-analyzer"]
                .get("initializationOptions")
                .is_none(),
            "must use opencode's 'initialization' key"
        );
    }

    #[test]
    fn materialize_lsp_empty_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        OpencodeAdapter
            .materialize(&MergedManifest::default(), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc.get("lsp").is_none());
    }

    #[test]
    fn materialize_lsp_disabled_server_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.lsp.push(llmenv_config::LspServer {
            name: "disabled-srv".into(),
            command: "some-ls".into(),
            disabled: true,
            ..Default::default()
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc.get("lsp").is_none());
    }

    #[test]
    fn materialize_permissions_allow_rule_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: None,
            paths: vec![],
        });
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allow = doc["permission"]["allow"].as_array().unwrap();
        assert!(allow.contains(&serde_json::json!("Bash")));
    }

    #[test]
    fn materialize_permissions_empty_when_no_rules() {
        let tmp = tempfile::tempdir().unwrap();
        OpencodeAdapter
            .materialize(&MergedManifest::default(), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc.get("permission").is_none());
    }

    #[test]
    fn materialize_native_opencode_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec!["Bash(echo*)".into()],
                ask: vec![],
                deny: vec![],
            },
        );
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allow = doc["permission"]["allow"].as_array().unwrap();
        assert!(allow.contains(&serde_json::json!("Bash(echo*)")));
    }

    #[test]
    fn materialize_native_opencode_rejects_modeled_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        let frag: serde_yaml::Value =
            serde_yaml::from_str("permission:\n  allow: [Bash]\n").unwrap();
        manifest.native.insert("opencode".into(), frag);
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("permission"),
            "error must name offending key: {err}"
        );
        assert!(
            err.to_string().contains("native_permissions"),
            "must point at correct channel"
        );
    }

    #[test]
    fn materialize_hook_unsupported_event_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.hooks.push(crate::config::Hook {
            event: "Notification".into(),
            matcher: None,
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::Command,
                command: Some("echo n".into()),
                tool: None,
            },
            bundle_origin: None,
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
        assert!(!shim_src.contains("\"event\":\"Notification\""));
    }

    #[test]
    fn materialize_shim_contains_auto_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        OpencodeAdapter
            .materialize(&MergedManifest::default(), tmp.path())
            .unwrap();
        let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
        assert!(shim_src.contains("check-stale --engine opencode"));
        assert!(shim_src.contains("config-context --engine opencode"));
        assert!(shim_src.contains("config-guard --engine opencode"));
    }

    #[test]
    fn materialize_hook_with_supported_event_rendered_in_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.hooks.push(crate::config::Hook {
            event: "PreToolUse".into(),
            matcher: None,
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::Command,
                command: Some("echo hi".into()),
                tool: None,
            },
            bundle_origin: None,
        });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
        assert!(shim_src.contains("echo hi"));
        assert!(shim_src.contains("check-stale --engine opencode"));
    }

    #[test]
    fn materialize_no_hooks_still_emits_shim_for_auto_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        OpencodeAdapter
            .materialize(&MergedManifest::default(), tmp.path())
            .unwrap();
        assert!(tmp.path().join("plugin/llmenv.js").exists());
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

        // 7. MCP servers
        if !manifest.mcps.is_empty() || manifest.capabilities.native_mcp.contains_key("opencode") {
            let mut mcp_obj = serde_json::Map::new();
            for mcp in &manifest.mcps {
                let mut e = match &mcp.kind {
                    ResolvedKind::Stdio { command, args, env } => {
                        let mut cmd: Vec<serde_json::Value> = Vec::with_capacity(1 + args.len());
                        cmd.push(serde_json::json!(command));
                        cmd.extend(args.iter().map(|a| serde_json::json!(a)));
                        let mut e = serde_json::Map::new();
                        e.insert("type".into(), serde_json::json!("local"));
                        e.insert("command".into(), serde_json::json!(cmd));
                        if !env.is_empty() {
                            e.insert("environment".into(), serde_json::json!(env));
                        }
                        e
                    }
                    ResolvedKind::Remote { url, transport: _ } => {
                        let mut e = serde_json::Map::new();
                        e.insert("type".into(), serde_json::json!("remote"));
                        e.insert("url".into(), serde_json::json!(url));
                        e
                    }
                };
                if !mcp.headers.is_empty() {
                    e.insert("headers".into(), serde_json::json!(mcp.headers));
                }
                if let Some(t) = mcp.timeout {
                    e.insert("timeout".into(), serde_json::json!(t));
                }
                mcp_obj.insert(mcp.name.clone(), serde_json::Value::Object(e));
            }
            // Overlay native_mcp.opencode
            let mut mcp_value = serde_json::Value::Object(mcp_obj);
            super::overlay_native_json(
                &mut mcp_value,
                manifest.capabilities.native_mcp.get("opencode"),
                "native_mcp.opencode",
            )?;
            if !mcp_value.as_object().is_none_or(serde_json::Map::is_empty) {
                doc.insert("mcp".into(), mcp_value);
            }
        }

        // 8. LSP servers
        if !manifest.capabilities.lsp.is_empty() {
            let mut lsp_obj = serde_json::Map::new();
            for srv in &manifest.capabilities.lsp {
                if srv.disabled {
                    continue;
                }
                let mut cmd: Vec<serde_json::Value> = Vec::with_capacity(1 + srv.args.len());
                cmd.push(serde_json::json!(srv.command));
                cmd.extend(srv.args.iter().map(|a| serde_json::json!(a)));
                let mut e = serde_json::Map::new();
                e.insert("command".into(), serde_json::json!(cmd));
                if !srv.filetypes.is_empty() {
                    e.insert("extensions".into(), serde_json::json!(srv.filetypes));
                }
                if !srv.env.is_empty() {
                    e.insert("env".into(), serde_json::json!(srv.env));
                }
                if let Some(opts) = &srv.init_options {
                    let as_json = serde_json::to_value(opts).map_err(|err| {
                        anyhow::anyhow!(
                            "LSP server '{}': failed to convert init_options to JSON: {err}",
                            srv.name
                        )
                    })?;
                    e.insert("initialization".into(), as_json);
                }
                lsp_obj.insert(srv.name.clone(), serde_json::Value::Object(e));
            }
            if !lsp_obj.is_empty() {
                doc.insert("lsp".into(), serde_json::Value::Object(lsp_obj));
            }
        }

        doc.insert(
            "$schema".into(),
            serde_json::json!("https://opencode.ai/config.json"),
        );
        if !instructions.is_empty() {
            doc.insert("instructions".into(), serde_json::json!(instructions));
        }

        // 9. Permissions — opencode uses `permission` (singular), with allow/ask/deny
        let perms = &manifest.capabilities.permissions;
        let native_perms = manifest.capabilities.native_permissions.get("opencode");

        let render_rules = |rules: &[crate::config::PermissionRule]| -> Vec<String> {
            rules
                .iter()
                .flat_map(|r| {
                    if let Some(pat) = &r.pattern {
                        vec![format!("{}({})", r.tool, pat)]
                    } else if !r.paths.is_empty() {
                        r.paths
                            .iter()
                            .map(|p| format!("{}({})", r.tool, p))
                            .collect()
                    } else {
                        vec![r.tool.clone()]
                    }
                })
                .collect()
        };

        let allowed = {
            let mut v = render_rules(&perms.allow);
            if let Some(n) = native_perms {
                v.extend(n.allow.iter().cloned());
            }
            dedup(&mut v);
            v
        };
        let asked = {
            let mut v = render_rules(&perms.ask);
            if let Some(n) = native_perms {
                v.extend(n.ask.iter().cloned());
            }
            dedup(&mut v);
            v
        };
        let denied = {
            let mut v = render_rules(&perms.deny);
            if let Some(n) = native_perms {
                v.extend(n.deny.iter().cloned());
            }
            dedup(&mut v);
            v
        };

        if !allowed.is_empty() || !asked.is_empty() || !denied.is_empty() {
            let mut perm_obj = serde_json::Map::new();
            if !allowed.is_empty() {
                perm_obj.insert("allow".into(), serde_json::json!(allowed));
            }
            if !asked.is_empty() {
                perm_obj.insert("ask".into(), serde_json::json!(asked));
            }
            if !denied.is_empty() {
                perm_obj.insert("deny".into(), serde_json::json!(denied));
            }
            doc.insert("permission".into(), serde_json::Value::Object(perm_obj));
        }

        // 10. Native overlay — reject modeled keys, then deep-merge
        const OPENCODE_MODELED_KEYS: &[&str] = &["instructions", "mcp", "lsp", "permission"];
        if let Some(native) = manifest.native.get("opencode") {
            super::reject_modeled_native_keys(native, OPENCODE_MODELED_KEYS, "opencode")?;
        }
        let mut doc_value = serde_json::Value::Object(doc);
        super::overlay_native_json(
            &mut doc_value,
            manifest.native.get("opencode"),
            "native.opencode",
        )?;
        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(OPENCODE_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;
        owned.push(PathBuf::from(OPENCODE_JSON_FILE));

        // 11. Hook shim — filter compatible events, warn on unsupported, generate JS plugin
        let compatible_hooks: Vec<&crate::config::Hook> = manifest
            .capabilities
            .hooks
            .iter()
            .filter(|hook| {
                if !SUPPORTED_HOOK_EVENTS.contains(&hook.event.as_str()) {
                    eprintln!(
                        "warning: opencode adapter does not support hook event '{}' — \
                         skipping this hook. Supported events: {}. Remove or move \
                         this hook to a claude_code-only bundle to silence this warning.",
                        hook.event,
                        SUPPORTED_HOOK_EVENTS.join(", ")
                    );
                    return false;
                }
                if matches!(hook.handler.kind, crate::config::HookHandlerKind::McpTool) {
                    eprintln!(
                        "warning: opencode adapter does not support mcp_tool hooks \
                         (hook event '{}', tool '{}') — skipping this hook. \
                         Use a command hook instead.",
                        hook.event,
                        hook.handler.tool.as_deref().unwrap_or("<unknown>")
                    );
                    return false;
                }
                true
            })
            .collect();

        let shim_js = generate_shim_js(&compatible_hooks);
        let plugin_dir = out.join("plugin");
        std::fs::create_dir_all(&plugin_dir)?;
        crate::paths::write_owner_only(&plugin_dir.join("llmenv.js"), shim_js.as_bytes())?;
        owned.push(PathBuf::from("plugin/llmenv.js"));

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

/// Build the JS source for `plugin/llmenv.js` — the hook bridge shim.
///
/// Each user-defined hook is mapped to its opencode event and bundled with
/// the three auto-hooks (`check-stale`, `config-context`, `config-guard`)
/// that always run on `SessionStart` / `PreToolUse`.
fn generate_shim_js(hooks: &[&crate::config::Hook]) -> String {
    let mut by_event: std::collections::BTreeMap<&str, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for hook in hooks {
        let resolved_command = hook
            .handler
            .command
            .as_deref()
            .map(|cmd| match &hook.bundle_origin {
                Some(bundle_dir) => super::resolve_bundle_relative_paths(cmd, bundle_dir)
                    .unwrap_or_else(|| cmd.to_string()),
                None => cmd.to_string(),
            })
            .unwrap_or_default();
        let timeout = 30_000u64;
        by_event
            .entry(hook.event.as_str())
            .or_default()
            .push(serde_json::json!({ "command": resolved_command, "timeout": timeout }));
    }

    // Auto-hooks — always present
    by_event
        .entry("SessionStart")
        .or_default()
        .push(serde_json::json!({
            "command": "llmenv check-stale --engine opencode",
            "timeout": 5000,
        }));
    by_event
        .entry("SessionStart")
        .or_default()
        .push(serde_json::json!({
            "command": "llmenv config-context --engine opencode",
            "timeout": 5000,
        }));
    by_event
        .entry("PreToolUse")
        .or_default()
        .push(serde_json::json!({
            "command": "llmenv config-guard --engine opencode",
            "timeout": 5000,
        }));

    let table: Vec<serde_json::Value> = by_event
        .into_iter()
        .map(|(event, commands)| {
            serde_json::json!({
                "event": event,
                "commands": commands,
            })
        })
        .collect();

    let table_json = serde_json::to_string(&table).expect("hook table must be valid JSON");
    SHIM_TEMPLATE.replace("${HOOK_TABLE}", &table_json)
}
