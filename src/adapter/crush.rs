use std::path::{Path, PathBuf};

use serde_json::json;

use super::AgentAdapter;
use crate::merge::MergedManifest;
use crate::util::{dedup, merge_json};

/// Adapter for Crush: writes `crush.json` into the cache dir and exports
/// `CRUSH_GLOBAL_CONFIG` / `CRUSH_GLOBAL_DATA` so Crush discovers it.
///
/// Hook support is limited to `PreToolUse`. Registering any other event is a
/// hard error — fail loudly rather than silently drop hooks.
#[derive(Debug, Default, Clone, Copy)]
pub struct CrushAdapter;

const CRUSH_JSON_FILE: &str = "crush.json";

/// Crush only supports PreToolUse hooks today.
const SUPPORTED_HOOK_EVENTS: &[&str] = &["PreToolUse"];

impl AgentAdapter for CrushAdapter {
    fn name(&self) -> &'static str {
        "crush"
    }

    fn binary_name(&self) -> &'static str {
        "crush"
    }

    fn supports_plugins(&self) -> bool {
        false
    }

    fn supports_lsp(&self) -> bool {
        true
    }

    fn supported_hook_events(&self) -> &'static [&'static str] {
        SUPPORTED_HOOK_EVENTS
    }

    fn env_vars(&self, cache_dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        Ok(vec![
            (
                "CRUSH_GLOBAL_CONFIG".into(),
                format!("{dir}/{CRUSH_JSON_FILE}"),
            ),
            ("CRUSH_GLOBAL_DATA".into(), dir.to_owned()),
        ])
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        // 1. Create output dir
        std::fs::create_dir_all(out)?;

        // 2. Validate hook events + hard-error on mcp_tool hooks (fix 5)
        for hook in &manifest.capabilities.hooks {
            if !SUPPORTED_HOOK_EVENTS.contains(&hook.event.as_str()) {
                anyhow::bail!(
                    "Crush adapter does not support hook event '{}'. \
                     Supported events: {}. Remove or move this hook to a \
                     claude_code-only bundle.",
                    hook.event,
                    SUPPORTED_HOOK_EVENTS.join(", ")
                );
            }
            if matches!(hook.handler.kind, crate::config::HookHandlerKind::McpTool) {
                anyhow::bail!(
                    "Crush adapter does not support mcp_tool hooks (hook event '{}', tool '{}'). \
                     Use a command hook instead.",
                    hook.event,
                    hook.handler.tool.as_deref().unwrap_or("<unknown>")
                );
            }
        }

        // 3. Write first-class skills (fix 2)
        let skill_paths =
            crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;

        // 4. Project plugin skills + hard-error on non-skill content (fix 4)
        let mut owned: Vec<PathBuf> = vec![PathBuf::from(CRUSH_JSON_FILE)];
        owned.extend(skill_paths.iter().cloned());

        let mut plugin_skill_paths: Vec<PathBuf> = Vec::new();
        for plugin in &manifest.plugins {
            let payload = resolve_plugin_payload(plugin, &manifest.marketplaces)?;
            for bad_dir in &["agents", "commands", "hooks"] {
                if payload.join(bad_dir).is_dir() {
                    anyhow::bail!(
                        "plugin '{}' contains unsupported Crush content: '{}/' directory \
                         — Crush has no equivalent for plugin agents, commands, or hooks. \
                         Scope this bundle away from Crush with `when:` or remove the content.",
                        plugin.plugin,
                        bad_dir
                    );
                }
            }
            let paths = crate::adapter::skills::project_plugin_skills(&payload, out)?;
            plugin_skill_paths.extend(paths);
        }
        owned.extend(plugin_skill_paths.iter().cloned());

        // P1-1: validate skills (frontmatter + hardcoded-path scan), same gate as ClaudeCodeAdapter
        crate::adapter::skills::validate_skills(out)?;

        // 5. Build doc
        let mut doc = serde_json::Map::new();

        // Hooks
        let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
            std::collections::BTreeMap::new();
        for hook in &manifest.capabilities.hooks {
            let handler = json!({
                "type": match hook.handler.kind {
                    crate::config::HookHandlerKind::Command => "command",
                    // P2-6: mcp_tool hooks are rejected at the validation gate above.
                    crate::config::HookHandlerKind::McpTool => unreachable!(
                        "mcp_tool hooks are rejected at the validation gate above"
                    ),
                },
                "command": hook.handler.command,
                "tool": hook.handler.tool,
            });
            let mut entry = serde_json::Map::new();
            if let Some(matcher) = &hook.matcher {
                entry.insert("matcher".into(), json!(matcher));
            }
            entry.insert("hooks".into(), json!([handler]));
            hooks_by_event
                .entry(hook.event.clone())
                .or_default()
                .push(serde_json::Value::Object(entry));
        }

        let mut hooks_value = serde_json::Value::Object(
            hooks_by_event
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect(),
        );
        overlay_native_crush(
            &mut hooks_value,
            manifest.capabilities.native_hooks.get("crush"),
        )?;
        // P1-4: validate every event key in the merged hooks object — native_hooks.crush can
        // inject unsupported events (e.g. PostToolUse) that bypass the earlier manifest gate.
        if let Some(obj) = hooks_value.as_object() {
            for event in obj.keys() {
                if !SUPPORTED_HOOK_EVENTS.contains(&event.as_str()) {
                    anyhow::bail!(
                        "native_hooks.crush contains unsupported hook event '{}'. \
                         Supported events: {}. Remove or move this hook.",
                        event,
                        SUPPORTED_HOOK_EVENTS.join(", ")
                    );
                }
            }
        }
        if !hooks_value
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
        {
            doc.insert("hooks".into(), hooks_value);
        }

        // Permissions: fail-closed — ask → deny (Crush has no ask concept)
        let perms = &manifest.capabilities.permissions;
        let native_perms = manifest.capabilities.native_permissions.get("crush");

        let mut allowed_tools = render_rules_to_strings(&perms.allow);
        if let Some(n) = native_perms {
            allowed_tools.extend(n.allow.iter().cloned());
        }

        let mut denied_tools = render_rules_to_strings(&perms.ask);
        denied_tools.extend(render_rules_to_strings(&perms.deny));
        if let Some(n) = native_perms {
            // ponytail: native ask → deny (fail-closed, same as neutral ask)
            denied_tools.extend(n.ask.iter().cloned());
            denied_tools.extend(n.deny.iter().cloned());
        }

        dedup(&mut allowed_tools);
        dedup(&mut denied_tools);

        let has_perms =
            !allowed_tools.is_empty() || !denied_tools.is_empty() || perms.default_mode.is_some();
        if has_perms {
            let mut perm_obj = serde_json::Map::new();
            if !allowed_tools.is_empty() {
                perm_obj.insert("allowed_tools".into(), json!(allowed_tools));
            }
            if !denied_tools.is_empty() {
                perm_obj.insert("denied_tools".into(), json!(denied_tools));
            }
            if let Some(mode) = perms.default_mode {
                perm_obj.insert("default_mode".into(), json!(crush_permission_mode(mode)));
            }
            doc.insert("permissions".into(), serde_json::Value::Object(perm_obj));
        }

        // MCP servers (fix 6: headers/timeout/disabled_tools)
        if !manifest.mcps.is_empty() || manifest.capabilities.native_mcp.contains_key("crush") {
            let mut mcp_obj = serde_json::Map::new();
            for mcp in &manifest.mcps {
                use crate::mcp::resolve::ResolvedKind;
                let mut e = match &mcp.kind {
                    ResolvedKind::Stdio { command, args, env } => {
                        let mut e = serde_json::Map::new();
                        e.insert("command".into(), json!(command));
                        e.insert("args".into(), json!(args));
                        if !env.is_empty() {
                            e.insert("env".into(), json!(env));
                        }
                        e
                    }
                    ResolvedKind::Remote { url, .. } => {
                        let mut e = serde_json::Map::new();
                        e.insert("type".into(), json!("remote"));
                        e.insert("url".into(), json!(url));
                        e
                    }
                };
                // Fields common to both transports (fix 6: parity).
                if !mcp.headers.is_empty() {
                    e.insert("headers".into(), json!(mcp.headers));
                }
                if let Some(t) = mcp.timeout {
                    e.insert("timeout".into(), json!(t));
                }
                if !mcp.disabled_tools.is_empty() {
                    e.insert("disabled_tools".into(), json!(mcp.disabled_tools));
                }
                mcp_obj.insert(mcp.name.clone(), serde_json::Value::Object(e));
            }
            // fix 7: overlay native_mcp.crush into the mcp object
            let mut mcp_value = serde_json::Value::Object(mcp_obj);
            overlay_native_crush(
                &mut mcp_value,
                manifest.capabilities.native_mcp.get("crush"),
            )?;
            if !mcp_value.as_object().is_none_or(serde_json::Map::is_empty) {
                doc.insert("mcp".into(), mcp_value);
            }
        }

        // LSP servers (fix 1): skip disabled servers; omit "lsp" key if none remain.
        if !manifest.capabilities.lsp.is_empty() {
            let lsp_value = render_lsp(&manifest.capabilities.lsp)?;
            if lsp_value.as_object().is_some_and(|o| !o.is_empty()) {
                doc.insert("lsp".into(), lsp_value);
            }
        }

        // options.skills_paths: emit whenever any skills exist (first-class or plugin-projected).
        // P1-2: must include plugin_skill_paths — plugin-only skill sets omit this key otherwise.
        if !skill_paths.is_empty() || !plugin_skill_paths.is_empty() {
            let skills_out = out
                .join("skills")
                .into_os_string()
                .into_string()
                .map_err(|p| {
                    anyhow::anyhow!(
                        "skills output path is not valid UTF-8: {}",
                        PathBuf::from(p).display()
                    )
                })?;
            let mut options_obj = serde_json::Map::new();
            options_obj.insert("skills_paths".into(), json!([skills_out]));
            doc.insert("options".into(), serde_json::Value::Object(options_obj));
        }

        // 6. native.crush passthrough — highest-precedence layer
        // P1-3: reject modeled keys in the catch-all fragment before overlaying — these
        // keys have dedicated rendering paths and must not clobber the security output.
        // Use native_permissions.crush / native_hooks.crush / native_mcp.crush instead.
        if let Some(native) = manifest.native.get("crush") {
            reject_modeled_keys_in_native_crush(native)?;
        }
        let mut doc_value = serde_json::Value::Object(doc);
        overlay_native_crush(&mut doc_value, manifest.native.get("crush"))?;

        // 7. Write crush.json
        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(CRUSH_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;

        Ok(owned)
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let wrapped = format!("[ICM MEMORY CONTEXT (auto-injected)]\n{text}");
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event_name,
                "additionalContext": wrapped
            }
        })
        .to_string()
    }
}

/// Resolve the on-disk payload directory for a plugin.
///
/// External plugins (`install_path = Some`) use that path directly.
/// First-party plugins look up their marketplace `install_location`.
fn resolve_plugin_payload(
    plugin: &crate::plugins::resolve::ResolvedPlugin,
    marketplaces: &[crate::plugins::resolve::ResolvedMarketplace],
) -> anyhow::Result<PathBuf> {
    // P2-5: guard traversal before any path join, regardless of which path is taken.
    if crate::paths::is_unsafe_join_target(&plugin.plugin) {
        anyhow::bail!("plugin name '{}' is unsafe (path traversal)", plugin.plugin);
    }
    if let Some(p) = &plugin.install_path {
        return Ok(PathBuf::from(p));
    }
    let mkt = marketplaces
        .iter()
        .find(|m| m.name == plugin.marketplace)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "plugin '{}': marketplace '{}' not found in resolved marketplaces",
                plugin.plugin,
                plugin.marketplace
            )
        })?;
    let install_location = mkt.install_location.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "plugin '{}': marketplace '{}' has no install_location (not yet synced?)",
            plugin.plugin,
            plugin.marketplace
        )
    })?;
    Ok(PathBuf::from(install_location).join(&plugin.plugin))
}

/// Build the `lsp` JSON object (keyed by server name) from a slice of LSP servers.
///
/// Disabled servers (`disabled == true`) are skipped entirely — Crush has no
/// way to model a conditionally-disabled server at runtime.
fn render_lsp(servers: &[llmenv_config::LspServer]) -> anyhow::Result<serde_json::Value> {
    let mut lsp_obj = serde_json::Map::new();
    for srv in servers {
        if srv.disabled {
            continue;
        }
        let mut e = serde_json::Map::new();
        e.insert("command".into(), json!(srv.command));
        if !srv.args.is_empty() {
            e.insert("args".into(), json!(srv.args));
        }
        if !srv.env.is_empty() {
            e.insert("env".into(), json!(srv.env));
        }
        if !srv.filetypes.is_empty() {
            e.insert("filetypes".into(), json!(srv.filetypes));
        }
        if !srv.root_markers.is_empty() {
            e.insert("root_markers".into(), json!(srv.root_markers));
        }
        if let Some(t) = srv.timeout {
            e.insert("timeout".into(), json!(t));
        }
        if let Some(opts) = &srv.init_options {
            let as_json = serde_json::to_value(opts).map_err(|err| {
                anyhow::anyhow!(
                    "LSP server '{}': failed to convert initializationOptions to JSON: {err}",
                    srv.name
                )
            })?;
            e.insert("initializationOptions".into(), as_json);
        }
        lsp_obj.insert(srv.name.clone(), serde_json::Value::Object(e));
    }
    Ok(serde_json::Value::Object(lsp_obj))
}

/// Keys that are fully modeled by CrushAdapter and must not appear in the `native.crush`
/// catch-all fragment. Overlaying them last would silently clobber the security-rendered
/// output (permissions, hooks) or the structured rendering (mcp, lsp).
///
/// Use the dedicated `native_permissions.crush` / `native_hooks.crush` / `native_mcp.crush`
/// channels instead, which merge in the safe direction.
const CRUSH_MODELED_KEYS: &[&str] = &["permissions", "hooks", "mcp", "lsp"];

/// P1-3: Reject `native.crush` fragments that carry modeled-feature keys.
fn reject_modeled_keys_in_native_crush(fragment: &serde_yaml::Value) -> anyhow::Result<()> {
    let Some(map) = fragment.as_mapping() else {
        return Ok(());
    };
    for key in CRUSH_MODELED_KEYS {
        if map.contains_key(serde_yaml::Value::String((*key).into())) {
            anyhow::bail!(
                "top-level `native.crush` carries the modeled-feature key `{key}`, \
                 which would silently clobber the rendered `{key}` \
                 (a security regression for permissions). \
                 Use `native_{key}.crush` (or `native_permissions.crush` / \
                 `native_hooks.crush` / `native_mcp.crush`) instead, \
                 which merges in the safe direction."
            );
        }
    }
    Ok(())
}

fn overlay_native_crush(
    dst: &mut serde_json::Value,
    fragment: Option<&serde_yaml::Value>,
) -> anyhow::Result<()> {
    if let Some(frag) = fragment {
        let as_json = serde_json::to_value(frag)
            .map_err(|e| anyhow::anyhow!("converting native crush fragment to JSON: {e}"))?;
        merge_json(dst, as_json);
    }
    Ok(())
}

fn render_rules_to_strings(rules: &[crate::config::PermissionRule]) -> Vec<String> {
    rules.iter().flat_map(render_permission_rule).collect()
}

fn render_permission_rule(rule: &crate::config::PermissionRule) -> Vec<String> {
    if let Some(pattern) = &rule.pattern {
        return vec![format!("{}({})", rule.tool, pattern)];
    }
    if !rule.paths.is_empty() {
        return rule
            .paths
            .iter()
            .map(|p| format!("{}({})", rule.tool, p))
            .collect();
    }
    vec![rule.tool.clone()]
}

fn crush_permission_mode(mode: crate::config::PermissionMode) -> &'static str {
    use crate::config::PermissionMode;
    match mode {
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Default => "default",
        PermissionMode::BypassPermissions => "bypass_permissions",
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::{
        CRUSH_JSON_FILE, CRUSH_MODELED_KEYS, CrushAdapter, SUPPORTED_HOOK_EVENTS,
        overlay_native_crush, reject_modeled_keys_in_native_crush, render_permission_rule,
    };
    use crate::adapter::AgentAdapter;
    use crate::config::{
        Capabilities, Hook, HookHandler, HookHandlerKind, NativePermissionRules, PermissionRule,
    };
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::merge::MergedManifest;
    use proptest::prelude::*;

    fn empty_manifest() -> MergedManifest {
        MergedManifest::default()
    }

    fn manifest_with_caps(caps: Capabilities) -> MergedManifest {
        MergedManifest {
            capabilities: caps,
            ..Default::default()
        }
    }

    fn pretooluse_hook(command: &str) -> Hook {
        Hook {
            event: "PreToolUse".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some(command.into()),
                tool: None,
            },
            bundle_origin: None,
        }
    }

    fn stdio_mcp(name: &str) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            kind: ResolvedKind::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "some-mcp".into()],
                env: std::collections::BTreeMap::new(),
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
        }
    }

    // ── env_vars ──────────────────────────────────────────────────────────────

    #[test]
    fn env_vars_returns_config_and_data() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(tmp.path()).unwrap();
        assert_eq!(vars.len(), 2);
        assert!(vars.iter().any(|(k, _)| k == "CRUSH_GLOBAL_CONFIG"));
        assert!(vars.iter().any(|(k, _)| k == "CRUSH_GLOBAL_DATA"));
    }

    #[test]
    fn env_vars_config_path_ends_with_crush_json() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(tmp.path()).unwrap();
        let (_, config) = vars
            .iter()
            .find(|(k, _)| k == "CRUSH_GLOBAL_CONFIG")
            .unwrap();
        assert!(
            config.ends_with("crush.json"),
            "expected crush.json in path, got {config}"
        );
    }

    #[test]
    fn env_vars_data_dir_is_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(tmp.path()).unwrap();
        let (_, data) = vars.iter().find(|(k, _)| k == "CRUSH_GLOBAL_DATA").unwrap();
        assert_eq!(data, tmp.path().to_str().unwrap());
    }

    // ── materialize: empty config ─────────────────────────────────────────────

    #[test]
    fn materialize_empty_config_writes_valid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let owned = CrushAdapter
            .materialize(&empty_manifest(), tmp.path())
            .unwrap();
        assert_eq!(owned, vec![std::path::PathBuf::from(CRUSH_JSON_FILE)]);
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(doc.is_object());
    }

    #[test]
    fn materialize_empty_config_produces_empty_object() {
        let tmp = tempfile::tempdir().unwrap();
        CrushAdapter
            .materialize(&empty_manifest(), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc, serde_json::json!({}));
    }

    // ── materialize: hooks ────────────────────────────────────────────────────

    #[test]
    fn materialize_pretooluse_hook_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(pretooluse_hook("echo hi"));
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc["hooks"]["PreToolUse"].is_array());
    }

    #[test]
    fn materialize_unsupported_hook_event_hard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(Hook {
            event: "SessionStart".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("echo start".into()),
                tool: None,
            },
            bundle_origin: None,
        });
        let err = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("SessionStart"),
            "error should name the unsupported event: {err}"
        );
    }

    #[test]
    fn materialize_unsupported_hook_includes_supported_list_in_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(Hook {
            event: "PostToolUse".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("echo post".into()),
                tool: None,
            },
            bundle_origin: None,
        });
        let err = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("PreToolUse"),
            "error should list supported events: {err}"
        );
    }

    // ── materialize: permissions ──────────────────────────────────────────────

    #[test]
    fn materialize_allow_rule_becomes_allowed_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.allow.push(PermissionRule {
            tool: "Bash".into(),
            pattern: None,
            paths: vec![],
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allowed = doc["permissions"]["allowed_tools"].as_array().unwrap();
        assert!(allowed.contains(&serde_json::json!("Bash")));
    }

    #[test]
    fn materialize_ask_rules_fail_closed_to_deny() {
        // ask → denied on Crush (fail-closed; Crush has no ask concept)
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.ask.push(PermissionRule {
            tool: "WebFetch".into(),
            pattern: None,
            paths: vec![],
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let denied = doc["permissions"]["denied_tools"].as_array().unwrap();
        assert!(denied.contains(&serde_json::json!("WebFetch")));
        let in_allowed = doc["permissions"]
            .get("allowed_tools")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|a| a.contains(&serde_json::json!("WebFetch")));
        assert!(!in_allowed, "ask rule must NOT appear in allowed_tools");
    }

    #[test]
    fn materialize_deny_rule_becomes_denied_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.deny.push(PermissionRule {
            tool: "Edit".into(),
            pattern: None,
            paths: vec![],
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let denied = doc["permissions"]["denied_tools"].as_array().unwrap();
        assert!(denied.contains(&serde_json::json!("Edit")));
    }

    #[test]
    fn materialize_permission_with_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.allow.push(PermissionRule {
            tool: "Bash".into(),
            pattern: Some("ls*".into()),
            paths: vec![],
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allowed = doc["permissions"]["allowed_tools"].as_array().unwrap();
        assert!(allowed.contains(&serde_json::json!("Bash(ls*)")));
    }

    // ── materialize: native passthrough ──────────────────────────────────────

    #[test]
    fn materialize_native_crush_merged_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = empty_manifest();
        let frag: serde_yaml::Value = serde_yaml::from_str("custom_key: custom_value").unwrap();
        manifest.native.insert("crush".into(), frag);
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["custom_key"], serde_json::json!("custom_value"));
    }

    // ── materialize: native_permissions passthrough ───────────────────────────

    #[test]
    fn materialize_native_permissions_allow_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.native_permissions.insert(
            "crush".into(),
            NativePermissionRules {
                allow: vec!["Bash(echo*)".into()],
                ask: vec![],
                deny: vec![],
            },
        );
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allowed = doc["permissions"]["allowed_tools"].as_array().unwrap();
        assert!(allowed.contains(&serde_json::json!("Bash(echo*)")));
    }

    #[test]
    fn materialize_native_permissions_ask_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.native_permissions.insert(
            "crush".into(),
            NativePermissionRules {
                allow: vec![],
                ask: vec!["Read(secret*)".into()],
                deny: vec![],
            },
        );
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let denied = doc["permissions"]["denied_tools"].as_array().unwrap();
        assert!(denied.contains(&serde_json::json!("Read(secret*)")));
    }

    // ── materialize: round-trip ───────────────────────────────────────────────

    #[test]
    fn materialize_roundtrip_json_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.allow.push(PermissionRule {
            tool: "Read".into(),
            pattern: Some("*.rs".into()),
            paths: vec![],
        });
        caps.hooks.push(Hook {
            event: "PreToolUse".into(),
            matcher: Some("^Bash$".into()),
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("llmenv throttle pre-tool".into()),
                tool: None,
            },
            bundle_origin: None,
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let doc2: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        assert_eq!(doc, doc2);
    }

    // ── emit_hook_context ─────────────────────────────────────────────────────

    #[test]
    fn emit_hook_context_empty_text_returns_empty() {
        assert_eq!(CrushAdapter.emit_hook_context("PreToolUse", ""), "");
    }

    #[test]
    fn emit_hook_context_wraps_in_hook_specific_output() {
        let out = CrushAdapter.emit_hook_context("PreToolUse", "some context");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("some context")
        );
    }

    #[test]
    fn emit_hook_context_includes_injection_barrier() {
        let out = CrushAdapter.emit_hook_context("PreToolUse", "mem");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            ctx.contains("[ICM MEMORY CONTEXT"),
            "missing injection barrier: {ctx}"
        );
    }

    // ── overlay_native_crush ──────────────────────────────────────────────────

    #[test]
    fn overlay_native_crush_none_is_noop() {
        let mut dst = serde_json::json!({ "k": 1 });
        let before = dst.clone();
        overlay_native_crush(&mut dst, None).unwrap();
        assert_eq!(dst, before);
    }

    #[test]
    fn overlay_native_crush_merges_keys() {
        let mut dst = serde_json::json!({ "a": 1 });
        let frag: serde_yaml::Value = serde_yaml::from_str("b: 2").unwrap();
        overlay_native_crush(&mut dst, Some(&frag)).unwrap();
        assert_eq!(dst["a"], serde_json::json!(1));
        assert_eq!(dst["b"], serde_json::json!(2));
    }

    // ── render_permission_rule ────────────────────────────────────────────────

    #[test]
    fn render_bare_tool() {
        let rule = PermissionRule {
            tool: "Bash".into(),
            pattern: None,
            paths: vec![],
        };
        assert_eq!(render_permission_rule(&rule), vec!["Bash"]);
    }

    #[test]
    fn render_tool_with_pattern() {
        let rule = PermissionRule {
            tool: "Bash".into(),
            pattern: Some("ls*".into()),
            paths: vec![],
        };
        assert_eq!(render_permission_rule(&rule), vec!["Bash(ls*)"]);
    }

    #[test]
    fn render_tool_with_paths() {
        let rule = PermissionRule {
            tool: "Read".into(),
            pattern: None,
            paths: vec!["src/".into(), "tests/".into()],
        };
        assert_eq!(
            render_permission_rule(&rule),
            vec!["Read(src/)", "Read(tests/)"]
        );
    }

    // ── constants ────────────────────────────────────────────────────────────

    #[test]
    fn supported_hook_events_contains_pretooluse() {
        assert!(SUPPORTED_HOOK_EVENTS.contains(&"PreToolUse"));
    }

    // ── materialize: LSP (fix 1) ──────────────────────────────────────────────

    #[test]
    fn materialize_lsp_server_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.lsp.push(llmenv_config::LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            args: vec!["--quiet".into()],
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["lsp"]["rust-analyzer"]["command"],
            serde_json::json!("rust-analyzer"),
            "LSP server command must be written"
        );
    }

    #[test]
    fn materialize_lsp_empty_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        CrushAdapter
            .materialize(&empty_manifest(), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("lsp").is_none(),
            "\"lsp\" key must be absent when no LSP servers configured"
        );
    }

    #[test]
    fn materialize_lsp_optional_fields_omitted_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.lsp.push(llmenv_config::LspServer {
            name: "tsserver".into(),
            command: "typescript-language-server".into(),
            // disabled=false, empty filetypes/root_markers/env, timeout=None
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let srv = &doc["lsp"]["tsserver"];
        assert!(
            srv.get("disabled").is_none(),
            "disabled=false must be omitted"
        );
        assert!(srv.get("env").is_none(), "empty env must be omitted");
        assert!(
            srv.get("filetypes").is_none(),
            "empty filetypes must be omitted"
        );
        assert!(
            srv.get("root_markers").is_none(),
            "empty root_markers must be omitted"
        );
        assert!(srv.get("timeout").is_none(), "None timeout must be omitted");
        assert!(
            srv.get("initializationOptions").is_none(),
            "None init_options must be omitted"
        );
    }

    // ── materialize: mcp_tool hook hard-errors (fix 5) ───────────────────────

    #[test]
    fn materialize_mcp_tool_hook_hard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(Hook {
            event: "PreToolUse".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::McpTool,
                command: None,
                tool: Some("my_tool".into()),
            },
            bundle_origin: None,
        });
        let err = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("mcp_tool"),
            "error must mention mcp_tool: {err}"
        );
    }

    // ── materialize: MCP headers/timeout/disabled_tools (fix 6) ─────────────

    #[test]
    fn materialize_mcp_headers_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mcp = stdio_mcp("srv");
        mcp.headers
            .insert("Authorization".into(), "Bearer tok".into());
        let mut manifest = empty_manifest();
        manifest.mcps.push(mcp);
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["mcp"]["srv"]["headers"]["Authorization"],
            serde_json::json!("Bearer tok"),
            "headers must be written into MCP entry"
        );
    }

    #[test]
    fn materialize_mcp_timeout_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mcp = stdio_mcp("srv");
        mcp.timeout = Some(30);
        let mut manifest = empty_manifest();
        manifest.mcps.push(mcp);
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["mcp"]["srv"]["timeout"],
            serde_json::json!(30),
            "timeout must be written into MCP entry"
        );
    }

    #[test]
    fn materialize_mcp_disabled_tools_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mcp = stdio_mcp("srv");
        mcp.disabled_tools = vec!["dangerous_tool".into()];
        let mut manifest = empty_manifest();
        manifest.mcps.push(mcp);
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let dt = doc["mcp"]["srv"]["disabled_tools"].as_array().unwrap();
        assert!(
            dt.contains(&serde_json::json!("dangerous_tool")),
            "disabled_tools must be written into MCP entry"
        );
    }

    // ── materialize: LSP disabled server omitted (fix 1) ─────────────────────

    #[test]
    fn materialize_lsp_disabled_server_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.lsp.push(llmenv_config::LspServer {
            name: "disabled-srv".into(),
            command: "some-ls".into(),
            disabled: true,
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("lsp").is_none(),
            "\"lsp\" key must be absent when all servers are disabled"
        );
    }

    // ── materialize: first-class skills (fix 2) ───────────────────────────────

    #[test]
    fn materialize_skills_written_and_paths_set() {
        let tmp = tempfile::tempdir().unwrap();
        // Set up a minimal skill source dir with a SKILL.md file.
        let skill_src = tempfile::tempdir().unwrap();
        std::fs::write(
            skill_src.path().join("SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill.\n---\n# MySkill\n",
        )
        .unwrap();

        let mut caps = Capabilities::default();
        caps.skills.push(crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.path().to_string_lossy().into_owned(),
            when: Vec::new(),
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // SKILL.md must be projected.
        assert!(
            tmp.path().join("skills/my-skill/SKILL.md").exists(),
            "SKILL.md must be written under out/skills/my-skill/"
        );
        // options.skills_paths must reference the skills dir.
        let skills_paths = doc["options"]["skills_paths"].as_array().unwrap();
        assert_eq!(skills_paths.len(), 1);
        let recorded = skills_paths[0].as_str().unwrap();
        assert!(
            recorded.ends_with("skills"),
            "skills_paths entry must end with 'skills', got: {recorded}"
        );
    }

    // ── materialize: plugin skill projection + agents/ hard-error (fix 3) ────

    #[test]
    fn materialize_plugin_skills_projected() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a fake plugin dir with a skills sub-directory.
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("skills/foo")).unwrap();
        std::fs::write(
            plugin_dir.path().join("skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: A foo skill.\n---\n# Foo\n",
        )
        .unwrap();

        let mut manifest = empty_manifest();
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "my-plugin".into(),
                collection: String::new(),
                install_path: Some(plugin_dir.path().to_string_lossy().into_owned()),
                git_commit_sha: None,
            });
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        assert!(
            tmp.path().join("skills/foo/SKILL.md").exists(),
            "plugin skill must be projected into out/skills/foo/"
        );
    }

    #[test]
    fn materialize_plugin_with_agents_hard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Plugin dir that contains an agents/ subdirectory.
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("agents")).unwrap();

        let mut manifest = empty_manifest();
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "bad-plugin".into(),
                collection: String::new(),
                install_path: Some(plugin_dir.path().to_string_lossy().into_owned()),
                git_commit_sha: None,
            });
        let err = CrushAdapter.materialize(&manifest, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("agents"),
            "error must name the unsupported 'agents' directory: {err}"
        );
        assert!(
            err.to_string().contains("bad-plugin"),
            "error must name the plugin: {err}"
        );
    }

    #[test]
    fn materialize_plugin_with_hooks_dir_hard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Plugin dir with a hooks/ subdirectory — spec §3.2 lists plugin-only
        // hooks as unsupported content that must hard-error, not silently drop.
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("hooks")).unwrap();

        let mut manifest = empty_manifest();
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "hooky-plugin".into(),
                collection: String::new(),
                install_path: Some(plugin_dir.path().to_string_lossy().into_owned()),
                git_commit_sha: None,
            });
        let err = CrushAdapter.materialize(&manifest, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("hooks"),
            "error must name the unsupported 'hooks' directory: {err}"
        );
        assert!(
            err.to_string().contains("hooky-plugin"),
            "error must name the plugin: {err}"
        );
    }

    // ── materialize: native_mcp.crush merged into mcp (fix 6) ────────────────

    #[test]
    fn materialize_native_mcp_crush_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        let frag: serde_yaml::Value =
            serde_yaml::from_str("injected-srv:\n  command: injected\n  args: []\n").unwrap();
        caps.native_mcp.insert("crush".into(), frag);
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["mcp"]["injected-srv"]["command"],
            serde_json::json!("injected"),
            "native_mcp.crush must be merged into the mcp section"
        );
    }

    // ── P1-1: validate_skills called by CrushAdapter ──────────────────────────

    #[test]
    fn materialize_skill_with_missing_skill_md_errors() {
        // A skill directory without SKILL.md must fail validate_skills.
        let tmp = tempfile::tempdir().unwrap();
        let skill_src = tempfile::tempdir().unwrap();
        // Write a file (not SKILL.md) to make it a non-empty dir.
        std::fs::write(skill_src.path().join("helper.sh"), "echo hi\n").unwrap();

        let mut caps = Capabilities::default();
        caps.skills.push(crate::config::SkillSource {
            name: "bad-skill".into(),
            path: skill_src.path().to_string_lossy().into_owned(),
            when: Vec::new(),
        });
        let err = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("SKILL.md"),
            "error must mention SKILL.md: {err}"
        );
    }

    // ── P1-2: plugin-only skills → skills_paths emitted ──────────────────────

    #[test]
    fn materialize_plugin_only_skills_emits_skills_paths() {
        // No first-class skills, only a plugin with a skills/ dir.
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("skills/my-skill")).unwrap();
        // Write a valid SKILL.md so validate_skills passes.
        std::fs::write(
            plugin_dir.path().join("skills/my-skill/SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill.\n---\n# My Skill\n",
        )
        .unwrap();

        let mut manifest = empty_manifest();
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "my-plugin".into(),
                collection: String::new(),
                install_path: Some(plugin_dir.path().to_string_lossy().into_owned()),
                git_commit_sha: None,
            });
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc["options"]["skills_paths"].is_array(),
            "options.skills_paths must be present when plugin-only skills exist: {doc}"
        );
    }

    // ── P1-3: native.crush modeled-key rejection ──────────────────────────────

    #[test]
    fn materialize_native_crush_with_permissions_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = empty_manifest();
        let frag: serde_yaml::Value =
            serde_yaml::from_str("permissions:\n  allowed_tools: [Bash]\n").unwrap();
        manifest.native.insert("crush".into(), frag);
        let err = CrushAdapter.materialize(&manifest, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("permissions"),
            "error must name the offending key: {err}"
        );
        assert!(
            err.to_string().contains("native_permissions"),
            "error must point at the correct channel: {err}"
        );
    }

    #[test]
    fn materialize_native_crush_with_hooks_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = empty_manifest();
        let frag: serde_yaml::Value = serde_yaml::from_str("hooks:\n  PreToolUse: []\n").unwrap();
        manifest.native.insert("crush".into(), frag);
        let err = CrushAdapter.materialize(&manifest, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("hooks"),
            "error must name the offending key: {err}"
        );
    }

    #[test]
    fn materialize_native_crush_custom_key_passes() {
        // Keys not in CRUSH_MODELED_KEYS must pass through unmolested.
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = empty_manifest();
        let frag: serde_yaml::Value =
            serde_yaml::from_str("telemetry:\n  enabled: false\n").unwrap();
        manifest.native.insert("crush".into(), frag);
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["telemetry"]["enabled"], serde_json::json!(false));
    }

    #[test]
    fn reject_modeled_keys_in_native_crush_all_modeled_keys_rejected() {
        for key in CRUSH_MODELED_KEYS {
            let frag: serde_yaml::Value =
                serde_yaml::from_str(&format!("{key}: anything")).unwrap();
            let err = reject_modeled_keys_in_native_crush(&frag).unwrap_err();
            assert!(
                err.to_string().contains(key),
                "error must name the offending key '{key}': {err}"
            );
        }
    }

    // ── P1-4: native_hooks.crush unsupported event rejection ─────────────────

    #[test]
    fn materialize_native_hooks_unsupported_event_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        // native_hooks.crush injects PostToolUse, which is unsupported.
        let frag: serde_yaml::Value =
            serde_yaml::from_str("PostToolUse:\n  - command: echo bad\n").unwrap();
        caps.native_hooks.insert("crush".into(), frag);
        let err = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("PostToolUse"),
            "error must name the offending event: {err}"
        );
        assert!(
            err.to_string().contains("PreToolUse"),
            "error must list supported events: {err}"
        );
    }

    #[test]
    fn materialize_native_hooks_supported_event_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        let frag: serde_yaml::Value = serde_yaml::from_str(
            "PreToolUse:\n  - hooks:\n      - type: command\n        command: echo ok\n",
        )
        .unwrap();
        caps.native_hooks.insert("crush".into(), frag);
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
    }

    // ── P2-5: resolve_plugin_payload traversal guard ──────────────────────────

    #[test]
    fn materialize_plugin_traversal_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = empty_manifest();
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "../escape".into(),
                collection: String::new(),
                // install_path=None forces the marketplace lookup path.
                // Use install_path=Some to test the join guard directly.
                install_path: None,
                git_commit_sha: None,
            });
        // We expect either a "marketplace not found" or a traversal error,
        // but NOT a silent success that would escape the install dir.
        // The traversal guard fires before the marketplace lookup when install_path=None
        // is not present — test with a fake marketplace to reach the join.
        // Easier: use install_path=Some with a traversal plugin name to verify the guard.
        let base = tempfile::tempdir().unwrap();
        manifest.plugins[0].install_path = Some(base.path().to_string_lossy().into_owned());
        // The join guard must fire before the plugin is resolved as a path.
        let err = CrushAdapter.materialize(&manifest, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("unsafe") || err.to_string().contains("traversal"),
            "error must name path traversal: {err}"
        );
    }

    // ── P2-7: proptest — render_lsp and emit_hook_context ────────────────────

    proptest! {
        #[test]
        fn prop_render_lsp_keys_match_non_disabled_servers(
            names in prop::collection::vec("[a-z][a-z0-9-]{0,15}", 0..6),
            disabled_flags in prop::collection::vec(proptest::bool::ANY, 0..6),
        ) {
            // Build LspServer list; zip names/flags (shortest wins).
            let servers: Vec<llmenv_config::LspServer> = names
                .iter()
                .zip(disabled_flags.iter())
                .map(|(n, &d)| llmenv_config::LspServer {
                    name: n.clone(),
                    command: "lang-server".into(),
                    disabled: d,
                    ..Default::default()
                })
                .collect();
            let expected: std::collections::BTreeSet<String> = servers
                .iter()
                .filter(|s| !s.disabled)
                .map(|s| s.name.clone())
                .collect();
            let result = super::render_lsp(&servers).unwrap();
            let got: std::collections::BTreeSet<String> = result
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            prop_assert_eq!(got, expected);
        }

        #[test]
        fn prop_emit_hook_context_non_empty_is_valid_json(
            event in "[A-Za-z]{1,20}",
            text in ".{1,200}",
        ) {
            let out = CrushAdapter.emit_hook_context(&event, &text);
            prop_assert!(
                serde_json::from_str::<serde_json::Value>(&out).is_ok(),
                "non-empty text must produce valid JSON; event={event}, text={text}, got={out}"
            );
        }

        #[test]
        fn prop_emit_hook_context_empty_text_is_empty_string(
            event in "[A-Za-z]{1,20}",
        ) {
            prop_assert_eq!(CrushAdapter.emit_hook_context(&event, ""), "");
        }
    }
}
