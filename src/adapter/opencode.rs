use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use super::AgentAdapter;
use crate::mcp::resolve::ResolvedKind;
use crate::merge::MergedManifest;

/// Adapter for opencode: writes `AGENTS.md` and `opencode.json` into the
/// cache dir and exports `OPENCODE_CONFIG_DIR` so opencode discovers them.
///
/// Skills use the claude-compatible `SKILL.md` format opencode reads natively.
/// Hooks are bridged via a generated `plugin/llmenv.js` shim (§3).
#[derive(Debug, Default, Clone, Copy)]
pub struct OpencodeAdapter;

const OPENCODE_JSON_FILE: &str = "opencode.json";

// ── Typed output structs for opencode.json ──

/// Top-level structure for the opencode.json config document.
/// Constructed from the merged manifest, serialized to Value, then native
/// overlay keys are deep-merged at the Value level.
#[derive(serde::Serialize)]
struct OpencodeConfig {
    /// Native opencode JS plugins (e.g. context-mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<Vec<String>>,
    /// MCP server configs — kept as Value because entries go through
    /// per-server native_mcp overlay after construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp: Option<serde_json::Value>,
    /// LSP server configs.
    #[serde(skip_serializing_if = "Option::is_none")]
    lsp: Option<BTreeMap<String, LspServerEntry>>,
    /// JSON Schema reference.
    #[serde(rename = "$schema")]
    schema: String,
    /// Paths to instruction files.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<Vec<String>>,
    /// Permission rules — per-tool pattern→action maps.
    #[serde(skip_serializing_if = "Option::is_none")]
    permission: Option<BTreeMap<String, PermissionValue>>,
}

/// An LSP server entry in opencode.json.
#[derive(serde::Serialize)]
struct LspServerEntry {
    /// Command with arguments.
    command: Vec<String>,
    /// File extensions this server handles (maps to opencode's `extensions`).
    #[serde(skip_serializing_if = "Option::is_none")]
    extensions: Option<Vec<String>>,
    /// Environment variables.
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<BTreeMap<String, String>>,
    /// Initialization options (maps to opencode's `initialization` key).
    #[serde(skip_serializing_if = "Option::is_none", rename = "initialization")]
    init_options: Option<serde_json::Value>,
}

/// A permission value — either a bare action string (when the tool has only
/// a wildcard pattern covering all inputs) or a pattern→action map (when the
/// tool has specific input patterns with distinct actions).
#[derive(serde::Serialize)]
#[serde(untagged)]
enum PermissionValue {
    /// Single action covering all patterns (e.g. `"allow"`).
    Simple(String),
    /// Mapping from input patterns to actions (e.g. `{"echo *": "allow", "rm *": "deny"}`).
    PatternMap(BTreeMap<String, String>),
}

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

// ── Plugin hooks manifest deserialization ──

/// A hooks.json or claude-codex-hooks.json manifest from a plugin hooks/ directory.
#[derive(serde::Deserialize)]
struct PluginHooksManifest {
    hooks: std::collections::HashMap<String, Vec<PluginHookGroup>>,
}

/// A group of hooks for a single event, with an optional matcher.
#[derive(serde::Deserialize)]
struct PluginHookGroup {
    #[serde(default)]
    matcher: Option<String>,
    hooks: Vec<PluginHookEntry>,
}

/// A single hook entry within a group.
#[derive(serde::Deserialize)]
struct PluginHookEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
}

/// Parse a plugin hooks manifest (hooks.json or claude-codex-hooks.json) into
/// [`crate::config::Hook`] structs.
///
/// Resolves `${CLAUDE_PLUGIN_ROOT}` to `payload_path` and only emits
/// `command`-type hooks (other types are silently skipped).
fn parse_plugin_hooks(
    hooks_path: &Path,
    payload_path: &Path,
    _plugin_name: &str,
) -> anyhow::Result<Vec<crate::config::Hook>> {
    let source = std::fs::read_to_string(hooks_path)?;
    let manifest: PluginHooksManifest = serde_json::from_str(&source)?;

    let mut hooks = Vec::new();
    for (event, groups) in &manifest.hooks {
        for group in groups {
            for entry in &group.hooks {
                let kind = entry.kind.as_deref().unwrap_or("command");
                if kind != "command" {
                    continue;
                }
                let Some(raw_command) = &entry.command else {
                    continue;
                };
                let resolved =
                    raw_command.replace("${CLAUDE_PLUGIN_ROOT}", &payload_path.to_string_lossy());
                hooks.push(crate::config::Hook {
                    event: event.clone(),
                    matcher: group.matcher.clone(),
                    handler: crate::config::HookHandler {
                        kind: crate::config::HookHandlerKind::Command,
                        command: Some(resolved),
                        tool: None,
                    },
                    bundle_origin: None,
                });
            }
        }
    }

    Ok(hooks)
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

    fn supports_model_providers(&self) -> bool {
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

        // 4. Plugin content translation (commands, agents, MCP, skills, hooks).
        let mut plugin_mcp_entries: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        let mut plugin_skill_paths: Vec<PathBuf> = Vec::new();
        let mut plugin_hooks: Vec<crate::config::Hook> = Vec::new();
        // Whether any plugin provides native opencode support (e.g. a
        // TypeScript plugin registered via "plugin" key in opencode.json).
        // Known: context-mode ships a native opencode plugin.
        let mut has_native_opencode = false;

        // Hoisted: command/ dir needed by plugin and bundle sections below.
        std::fs::create_dir_all(out.join("command"))?;

        for plugin in &manifest.plugins {
            let payload = super::crush::resolve_plugin_payload(plugin, &manifest.marketplaces)?;

            // Does this plugin provide native opencode support (e.g. a
            // TypeScript plugin registered via "plugin" key in opencode.json)?
            // Known: context-mode ships a native opencode plugin.
            let is_native_opencode = plugin.plugin == "context-mode";

            // 4a. Plugin-provided MCP (LLM_PROVIDER_MCP_JSON). Skip for native
            //     opencode plugins — they register their own tools internally
            //     and including MCP entries would duplicate them.
            if is_native_opencode {
                has_native_opencode = true;
            } else {
                let mcp_json_path = payload.join("LLM_PROVIDER_MCP_JSON");
                if mcp_json_path.exists() {
                    let content = std::fs::read_to_string(&mcp_json_path)?;
                    let yaml_value: serde_yaml::Value =
                        serde_yaml::from_str(&content).map_err(|e| {
                            anyhow::anyhow!(
                                "plugin '{}': failed to parse LLM_PROVIDER_MCP_JSON: {e}",
                                plugin.plugin
                            )
                        })?;
                    if let Some(obj) = yaml_value.as_mapping() {
                        let json_value: serde_json::Value = serde_json::to_value(obj)?;
                        if let Some(json_obj) = json_value.as_object() {
                            for (k, v) in json_obj {
                                plugin_mcp_entries.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }

            // 4b. Translate commands/ from plugin
            let cmd_dir = payload.join("commands");
            if cmd_dir.exists() {
                for entry in std::fs::read_dir(&cmd_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "md") {
                        continue;
                    }
                    let name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                        anyhow::anyhow!(
                            "plugin '{}': command file '{}' has no valid file stem",
                            plugin.plugin,
                            path.display(),
                        )
                    })?;
                    let source = std::fs::read_to_string(&path)?;
                    let translated = translate_command_md(&source, name)?;
                    let out_name = format!("command/__plugin_{name}.md");
                    crate::paths::write_owner_only(&out.join(&out_name), translated.as_bytes())?;
                    plugin_skill_paths.push(PathBuf::from(&out_name));
                }
            }

            // 4c. Translate agents/ from plugin
            let agent_dir = payload.join("agents");
            if agent_dir.exists() {
                for entry in std::fs::read_dir(&agent_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "md") {
                        continue;
                    }
                    let name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                        anyhow::anyhow!(
                            "plugin '{}': agent file '{}' has no valid file stem",
                            plugin.plugin,
                            path.display(),
                        )
                    })?;
                    let source = std::fs::read_to_string(&path)?;
                    let translated = translate_agent_md(&source, name)?;
                    let out_name = format!("command/__plugin_agent_{name}.md");
                    crate::paths::write_owner_only(&out.join(&out_name), translated.as_bytes())?;
                    plugin_skill_paths.push(PathBuf::from(&out_name));
                }
            }

            // 4d. Plugin-projected skills (skills dir inside a plugin payload).
            let paths = crate::adapter::skills::project_plugin_skills(&payload, out)?;
            plugin_skill_paths.extend(paths);

            // 4e. Plugin hooks from hooks/ dir — translate to top-level hooks.
            //     Skip for native opencode plugins — they handle hooks internally.
            if !is_native_opencode {
                let hooks_dir = payload.join("hooks");
                for manifest_name in ["hooks.json", "claude-codex-hooks.json"] {
                    let hooks_path = hooks_dir.join(manifest_name);
                    if !hooks_path.exists() {
                        continue;
                    }
                    let mut parsed = parse_plugin_hooks(&hooks_path, &payload, &plugin.plugin)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "failed to parse '{}' for plugin '{}': {e}",
                                hooks_path.display(),
                                plugin.plugin,
                            )
                        })?;
                    // Filter to supported events and command-only kinds at collection
                    // time, same as §11 does for manifest hooks.
                    parsed.retain(|hook| {
                        if !SUPPORTED_HOOK_EVENTS.contains(&hook.event.as_str()) {
                            eprintln!(
                                "warning: opencode adapter does not support hook event \
                             '{}' from plugin '{}' — skipping. Supported events: {}.",
                                hook.event,
                                plugin.plugin,
                                SUPPORTED_HOOK_EVENTS.join(", ")
                            );
                            return false;
                        }
                        if matches!(hook.handler.kind, crate::config::HookHandlerKind::McpTool) {
                            eprintln!(
                                "warning: opencode adapter does not support mcp_tool hook \
                             from plugin '{}' — skipping.",
                                plugin.plugin,
                            );
                            return false;
                        }
                        true
                    });
                    plugin_hooks.extend(parsed);
                    break; // only process one hooks manifest (no duplicates)
                }
            }
        }

        owned.extend(plugin_skill_paths);

        // 5. Validate skills (frontmatter + hardcoded-path scan).
        crate::adapter::skills::validate_skills(out)?;

        // 6. Build opencode.json with what we have so far
        // Register native opencode TypeScript plugins (e.g. context-mode) that
        // ship their own hook/tool implementations independent of the llmenv JS
        // shim. These are resolved by opencode's plugin system at startup.
        let config_plugin = if has_native_opencode {
            Some(vec!["context-mode".into()])
        } else {
            None
        };

        // 7. MCP servers
        let config_mcp = {
            if !manifest.mcps.is_empty()
                || manifest.capabilities.native_mcp.contains_key("opencode")
            {
                let mut mcp_obj = serde_json::Map::new();
                for mcp in &manifest.mcps {
                    let mut e = match &mcp.kind {
                        ResolvedKind::Stdio { command, args, env } => {
                            let mut cmd: Vec<serde_json::Value> =
                                Vec::with_capacity(1 + args.len());
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
                // Merge plugin-provided MCP entries (from LLM_PROVIDER_MCP_JSON).
                for (k, v) in &plugin_mcp_entries {
                    if mcp_obj.contains_key(k) {
                        eprintln!(
                            "warning: duplicate MCP server '{k}' from plugin — \
                             last definition wins"
                        );
                    }
                    mcp_obj.insert(k.clone(), v.clone());
                }
                // Overlay native_mcp.opencode
                let mut mcp_value = serde_json::Value::Object(mcp_obj);
                super::overlay_native_json(
                    &mut mcp_value,
                    manifest.capabilities.native_mcp.get("opencode"),
                    "native_mcp.opencode",
                )?;
                if mcp_value.as_object().is_none_or(serde_json::Map::is_empty) {
                    None
                } else {
                    Some(mcp_value)
                }
            } else {
                None
            }
        };

        // 8. LSP servers
        let config_lsp = {
            if !manifest.capabilities.lsp.is_empty() {
                let mut lsp_entries: BTreeMap<String, LspServerEntry> = BTreeMap::new();
                for srv in &manifest.capabilities.lsp {
                    if srv.disabled {
                        continue;
                    }
                    let cmd: Vec<String> = std::iter::once(srv.command.clone())
                        .chain(srv.args.iter().cloned())
                        .collect();
                    let init_options = match &srv.init_options {
                        Some(opts) => Some(serde_json::to_value(opts).map_err(|err| {
                            anyhow::anyhow!(
                                "LSP server '{}': failed to convert init_options to JSON: {err}",
                                srv.name
                            )
                        })?),
                        None => None,
                    };
                    lsp_entries.insert(
                        srv.name.clone(),
                        LspServerEntry {
                            command: cmd,
                            extensions: if srv.filetypes.is_empty() {
                                None
                            } else {
                                Some(srv.filetypes.clone())
                            },
                            env: if srv.env.is_empty() {
                                None
                            } else {
                                Some(srv.env.clone())
                            },
                            init_options,
                        },
                    );
                }
                if lsp_entries.is_empty() {
                    None
                } else {
                    Some(lsp_entries)
                }
            } else {
                None
            }
        };

        let config_instructions = if !instructions.is_empty() {
            Some(instructions)
        } else {
            None
        };

        // 9. Permissions — opencode uses `permission` with per-tool pattern→action maps.
        //     Format: { "bash": { "otool *": "allow", "rm *": "deny" } }
        let perms = &manifest.capabilities.permissions;
        let native_perms = manifest.capabilities.native_permissions.get("opencode");

        let mut permission_map: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, String>,
        > = std::collections::BTreeMap::new();

        // Convert a PermissionRule into (tool_lowercase, pattern) pairs.
        // Rules with a pattern use it; rules with paths use each path as a pattern;
        // bare rules (no pattern, no paths) wildcard-match everything for the tool.
        fn rule_to_patterns(rule: &crate::config::PermissionRule) -> Vec<(String, String)> {
            if let Some(pat) = &rule.pattern {
                vec![(rule.tool.to_ascii_lowercase(), pat.clone())]
            } else if !rule.paths.is_empty() {
                rule.paths
                    .iter()
                    .map(|p| (rule.tool.to_ascii_lowercase(), p.clone()))
                    .collect()
            } else {
                vec![(rule.tool.to_ascii_lowercase(), "*".to_string())]
            }
        }

        // Parse a native string like "Bash(otool *)" or bare "Bash" back into
        // (tool_lowercase, pattern). A bare tool name wildcard-matches everything.
        fn parse_native_rule(s: &str) -> (String, String) {
            if let Some(start) = s.find('(')
                && let Some(end) = s.rfind(')')
            {
                return (
                    s[..start].to_ascii_lowercase(),
                    s[start + 1..end].to_string(),
                );
            }
            (s.to_ascii_lowercase(), "*".to_string())
        }

        // Insert (tool, pattern) pairs into the per-tool map.
        // Deny overrides ask overrides allow for the same tool+pattern
        // (last-write-wins).
        fn insert_patterns(
            map: &mut std::collections::BTreeMap<
                String,
                std::collections::BTreeMap<String, String>,
            >,
            iter: impl Iterator<Item = (String, String)>,
            action: &str,
        ) {
            for (tool, pattern) in iter {
                map.entry(tool)
                    .or_default()
                    .insert(pattern, action.to_string());
            }
        }

        // Insertion order: structured rules first, then native rules. Native rules
        // use last-write-wins at the pattern level (BTreeMap insertion),
        // so a native wildcard `*` for a tool shadows ALL structured patterns
        // for that tool. If a user has `allow: [Bash(otool *)]` AND a native
        // `deny: [Bash(*)]`, the final state is `Bash/* -> deny` because the
        // native insertion runs after the structured one. Use one category
        // (structured OR native) per tool to avoid unexpected shadowing.
        insert_patterns(
            &mut permission_map,
            perms.allow.iter().flat_map(rule_to_patterns),
            "allow",
        );
        insert_patterns(
            &mut permission_map,
            perms.ask.iter().flat_map(rule_to_patterns),
            "ask",
        );
        insert_patterns(
            &mut permission_map,
            perms.deny.iter().flat_map(rule_to_patterns),
            "deny",
        );
        if let Some(n) = native_perms {
            insert_patterns(
                &mut permission_map,
                n.allow.iter().map(|s| parse_native_rule(s)),
                "allow",
            );
            insert_patterns(
                &mut permission_map,
                n.ask.iter().map(|s| parse_native_rule(s)),
                "ask",
            );
            insert_patterns(
                &mut permission_map,
                n.deny.iter().map(|s| parse_native_rule(s)),
                "deny",
            );
        }

        let config_permission = if !permission_map.is_empty() {
            let mut perm_entries: BTreeMap<String, PermissionValue> = BTreeMap::new();
            for (tool, patterns) in &permission_map {
                // Bare tool (single wildcard pattern) -> emit action string directly.
                // Tool with specific patterns -> emit pattern->action object.
                if patterns.len() == 1 && patterns.contains_key("*") {
                    perm_entries
                        .insert(tool.clone(), PermissionValue::Simple(patterns["*"].clone()));
                } else {
                    let mut pmap: BTreeMap<String, String> = BTreeMap::new();
                    for (pattern, action) in patterns {
                        pmap.insert(pattern.clone(), action.clone());
                    }
                    perm_entries.insert(tool.clone(), PermissionValue::PatternMap(pmap));
                }
            }
            Some(perm_entries)
        } else {
            None
        };

        // 10. Native overlay — reject modeled keys, then deep-merge
        const OPENCODE_MODELED_KEYS: &[&str] = &["instructions", "mcp", "lsp", "permission"];
        if let Some(native) = manifest.native.get("opencode") {
            super::reject_modeled_native_keys(native, OPENCODE_MODELED_KEYS, "opencode")?;
        }
        let config = OpencodeConfig {
            plugin: config_plugin,
            mcp: config_mcp,
            lsp: config_lsp,
            schema: "https://opencode.ai/config.json".into(),
            instructions: config_instructions,
            permission: config_permission,
        };
        let mut doc_value = serde_json::to_value(&config)?;
        super::overlay_native_json(
            &mut doc_value,
            manifest.native.get("opencode"),
            "native.opencode",
        )?;
        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(OPENCODE_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;
        owned.push(PathBuf::from(OPENCODE_JSON_FILE));

        // 11. Hook shim — merge manifest hooks + plugin hooks, generate JS plugin
        let mut all_hooks: Vec<crate::config::Hook> = manifest
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
            .cloned()
            .collect();
        all_hooks.extend(plugin_hooks);

        let shim_js = generate_shim_js(&all_hooks)?;
        let plugin_dir = out.join("plugin");
        std::fs::create_dir_all(&plugin_dir)?;
        crate::paths::write_owner_only(&plugin_dir.join("llmenv.js"), shim_js.as_bytes())?;
        owned.push(PathBuf::from("plugin/llmenv.js"));

        // 12. Bundle-level commands/agents from manifest.files
        for (rel, abs) in &manifest.files {
            let rel_str = rel.to_string_lossy();
            if rel_str.starts_with("commands/") && rel_str.ends_with(".md") {
                let source = std::fs::read_to_string(abs).map_err(|e| {
                    anyhow::anyhow!("failed to read bundle file '{}': {e}", rel.display())
                })?;
                let name = rel.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "bundle command file '{}' has no valid file stem",
                        rel.display(),
                    )
                })?;
                let translated = translate_command_md(&source, name)?;
                let out_name = format!("command/__bundle_{name}.md");
                crate::paths::write_owner_only(&out.join(&out_name), translated.as_bytes())?;
                owned.push(PathBuf::from(&out_name));
            }
            if rel_str.starts_with("agents/") && rel_str.ends_with(".md") {
                let source = std::fs::read_to_string(abs).map_err(|e| {
                    anyhow::anyhow!("failed to read bundle file '{}': {e}", rel.display())
                })?;
                let name = rel.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "bundle agent file '{}' has no valid file stem",
                        rel.display(),
                    )
                })?;
                let translated = translate_agent_md(&source, name)?;
                let out_name = format!("command/__bundle_agent_{name}.md");
                crate::paths::write_owner_only(&out.join(&out_name), translated.as_bytes())?;
                owned.push(PathBuf::from(&out_name));
            }
        }

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
fn generate_shim_js(hooks: &[crate::config::Hook]) -> anyhow::Result<String> {
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

    let table_json = serde_json::to_string(&table)?;
    Ok(SHIM_TEMPLATE.replace("${HOOK_TABLE}", &table_json))
}

/// Split markdown source into optional frontmatter and body.
///
/// Returns `(parsed_frontmatter_or_None, body_text)`. The body text is the
/// content after the closing `---` delimiter (leading whitespace trimmed).
fn split_frontmatter(source: &str) -> (Option<serde_yaml::Value>, &str) {
    let s = source.trim_start();
    if !s.starts_with("---") {
        return (None, source);
    }
    let after_opener = &s[3..];

    // Find the closing `\n---` (opener + newline + closing delimiter).
    if let Some(end) = after_opener.find("\n---") {
        // `end` is the position of `\n` before the closing `---`.
        // YAML content is after the opening `---\n` (position 1 of after_opener)
        // and before the `\n` at position `end`.
        let fm_raw = if end > 0 { &after_opener[1..end] } else { "" };
        let body = &after_opener[(end + 4)..];
        let fm = serde_yaml::from_str(fm_raw).ok();
        (fm, body.trim_start())
    } else {
        (None, source)
    }
}

/// Translate a command markdown file from Claude frontmatter to opencode format.
///
/// Keeps only `description` from the frontmatter. Drops `model` and
/// `allowed_tools` with a warning — those are Claude-specific concepts that
/// opencode does not support for commands.
fn translate_command_md(source: &str, _name: &str) -> anyhow::Result<String> {
    let (fm, body) = split_frontmatter(source);
    let mut new_fm = serde_yaml::Mapping::new();

    if let Some(serde_yaml::Value::Mapping(ref map)) = fm {
        if let Some(desc) = map.get(serde_yaml::Value::String("description".into())) {
            new_fm.insert("description".into(), desc.clone());
        }
        let dropped: &[&str] = &["model", "allowed_tools"];
        for key in dropped {
            if map.contains_key(serde_yaml::Value::String((*key).into())) {
                eprintln!(
                    "warning: opencode adapter does not support '{key}' in \
                     command frontmatter — dropping this field"
                );
            }
        }
    }

    if new_fm.is_empty() {
        Ok(body.to_string())
    } else {
        let fm_yaml = serde_yaml::to_string(&new_fm)?;
        Ok(format!("---\n{}---\n{}", fm_yaml, body))
    }
}

/// Translate an agent markdown file from Claude frontmatter to opencode format.
///
/// Keeps `description`, `model`, `tools` / `allowed_tools`, and adds
/// `mode: subagent` so opencode runs the agent as a sub-process.
fn translate_agent_md(source: &str, _name: &str) -> anyhow::Result<String> {
    let (fm, body) = split_frontmatter(source);
    let mut new_fm = serde_yaml::Mapping::new();

    if let Some(serde_yaml::Value::Mapping(ref map)) = fm {
        let keep: &[&str] = &["description", "model"];
        for key in keep {
            if let Some(val) = map.get(serde_yaml::Value::String((*key).into())) {
                new_fm.insert((*key).into(), val.clone());
            }
        }
        for tool_key in &["tools", "allowed_tools"] {
            if let Some(val) = map.get(serde_yaml::Value::String((*tool_key).into())) {
                new_fm.insert((*tool_key).into(), val.clone());
            }
        }
    }

    new_fm.insert("mode".into(), serde_yaml::Value::String("subagent".into()));
    let fm_yaml = serde_yaml::to_string(&new_fm)?;
    Ok(format!("---\n{}---\n{}", fm_yaml, body))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
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
        let manifest = MergedManifest {
            agents_md: "# Test Rules\n\nSome content here.".to_string(),
            ..Default::default()
        };
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

        let manifest = MergedManifest {
            plugins: vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: "test".into(),
                plugin: "my-plugin".into(),
                collection: String::new(),
                install_path: Some(plugin_dir.path().to_str().unwrap().into()),
                git_commit_sha: None,
            }],
            marketplaces: vec![crate::plugins::resolve::ResolvedMarketplace {
                name: "test".into(),
                source: String::new(),
                install_location: None,
                head: None,
            }],
            ..Default::default()
        };
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
        // Bare tool (no pattern, no paths) -> emitted as action string directly:
        // doc["permission"]["bash"] = "allow"
        assert_eq!(doc["permission"]["bash"], serde_json::json!("allow"));
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
        // Old format: doc["permission"]["allow"] as array with "Bash(echo*)"
        // New format: doc["permission"]["bash"]["echo*"] = "allow"
        let bash = &doc["permission"]["bash"];
        assert_eq!(bash["echo*"], serde_json::json!("allow"));
    }

    #[test]
    fn materialize_permissions_mixed_allow_deny_same_tool() {
        // Regression test: user config had Bash allow and Bash deny rules with
        // patterns. Old format emitted flat arrays; OpenCode 1.17.15 expects
        // per-tool pattern→action maps.
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: Some("otool *".into()),
            paths: vec![],
        });
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "Read".into(),
            pattern: None,
            paths: vec![],
        });
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: Some("rm *".into()),
            paths: vec![],
        });
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "Edit".into(),
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
        // Verify per-tool format: each tool key maps patterns to actions

        // Bash has specific patterns -> emitted as pattern->action object
        let bash = &doc["permission"]["bash"];
        assert_eq!(bash["otool *"], serde_json::json!("allow"));
        assert_eq!(bash["rm *"], serde_json::json!("deny"));

        // Bare tools without patterns -> emitted as action string directly
        assert_eq!(doc["permission"]["read"], serde_json::json!("allow"));
        assert_eq!(doc["permission"]["edit"], serde_json::json!("deny"));

        // No flat "allow"/"deny"/"ask" arrays at the permission level
        assert!(doc["permission"].get("allow").is_none());
        assert!(doc["permission"].get("deny").is_none());
        assert!(doc["permission"].get("ask").is_none());
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

    #[test]
    fn translate_command_md_keeps_description() {
        let src = "---\ndescription: My test command\nmodel: claude-sonnet-4-20250514\nallowed_tools: [Bash]\n---\n\nRun this command.";
        let result = translate_command_md(src, "test").unwrap();
        assert!(result.contains("description: My test command"));
        assert!(!result.contains("model:"));
        assert!(!result.contains("allowed_tools:"));
        assert!(result.contains("Run this command."));
    }

    #[test]
    fn translate_command_md_no_frontmatter_passthrough() {
        let src = "Just raw markdown content.";
        let result = translate_command_md(src, "test").unwrap();
        assert_eq!(result, "Just raw markdown content.");
    }

    #[test]
    fn translate_command_md_empty_description_still_works() {
        let src = "---\n---\n\nBody only.";
        let result = translate_command_md(src, "test").unwrap();
        // No valid frontmatter fields → body returned as-is (no --- wrapper)
        assert_eq!(result, "Body only.");
    }

    #[test]
    fn translate_agent_md_keeps_model_description_tools() {
        let src = "---\ndescription: My agent\nmodel: claude-sonnet-4-20250514\ntools: [Bash, Read]\n---\n\nDo things autonomously.";
        let result = translate_agent_md(src, "test").unwrap();
        assert!(result.contains("description: My agent"));
        assert!(result.contains("model: claude-sonnet-4-20250514"));
        assert!(result.contains("tools:"));
        assert!(result.contains("mode: subagent"));
        assert!(result.contains("Do things autonomously."));
    }

    #[test]
    fn translate_agent_md_adds_subagent_mode() {
        let src = "---\ndescription: Just description\n---\n\nBody.";
        let result = translate_agent_md(src, "test").unwrap();
        assert!(result.contains("description: Just description"));
        assert!(result.contains("mode: subagent"));
    }

    #[test]
    fn translate_agent_md_no_frontmatter_passthrough() {
        let src = "Just raw content.";
        let result = translate_agent_md(src, "test").unwrap();
        assert!(result.contains("mode: subagent"));
        assert!(result.contains("Just raw content."));
    }

    #[test]
    fn translate_agent_md_accepts_allowed_tools() {
        let src = "---\ndescription: Agent with allowed tools\nallowed_tools: [Bash]\n---\n\nBody.";
        let result = translate_agent_md(src, "test").unwrap();
        assert!(result.contains("allowed_tools:"));
        assert!(result.contains("mode: subagent"));
    }

    #[test]
    fn materialize_bundle_command_from_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();

        let src_dir = tempfile::tempdir().unwrap();
        let cmd_file = src_dir.path().join("test-cmd.md");
        std::fs::write(
            &cmd_file,
            "---\ndescription: A test command\nmodel: claude-sonnet-4-20250514\n---\n\nRun this thing.",
        )
        .unwrap();

        manifest
            .files
            .insert(PathBuf::from("commands/test-cmd.md"), cmd_file);

        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();

        let out_cmd = tmp.path().join("command/__bundle_test-cmd.md");
        assert!(out_cmd.exists());
        let content = std::fs::read_to_string(&out_cmd).unwrap();
        assert!(content.contains("description: A test command"));
        assert!(!content.contains("model:")); // dropped for commands
        assert!(content.contains("Run this thing."));
    }

    #[test]
    fn env_vars_returns_opencode_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = OpencodeAdapter.env_vars(tmp.path(), tmp.path()).unwrap();
        assert!(vars.contains(&(
            "OPENCODE_CONFIG_DIR".into(),
            tmp.path().to_string_lossy().into_owned()
        )));
    }
}
