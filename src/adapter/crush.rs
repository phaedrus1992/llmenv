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
        std::fs::create_dir_all(out)?;

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
        }

        let mut doc = serde_json::Map::new();

        // Hooks
        let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
            std::collections::BTreeMap::new();
        for hook in &manifest.capabilities.hooks {
            let handler = json!({
                "type": match hook.handler.kind {
                    crate::config::HookHandlerKind::Command => "command",
                    crate::config::HookHandlerKind::McpTool => "mcp_tool",
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

        // MCP servers
        if !manifest.mcps.is_empty() {
            let mut mcp_obj = serde_json::Map::new();
            for mcp in &manifest.mcps {
                use crate::mcp::resolve::ResolvedKind;
                let entry = match &mcp.kind {
                    ResolvedKind::Stdio { command, args, env } => {
                        let mut e = serde_json::Map::new();
                        e.insert("command".into(), json!(command));
                        e.insert("args".into(), json!(args));
                        if !env.is_empty() {
                            e.insert("env".into(), json!(env));
                        }
                        serde_json::Value::Object(e)
                    }
                    ResolvedKind::Remote { url, .. } => {
                        json!({ "type": "remote", "url": url })
                    }
                };
                mcp_obj.insert(mcp.name.clone(), entry);
            }
            doc.insert("mcp".into(), serde_json::Value::Object(mcp_obj));
        }

        // native.crush passthrough — highest-precedence layer
        let mut doc_value = serde_json::Value::Object(doc);
        overlay_native_crush(&mut doc_value, manifest.native.get("crush"))?;

        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(CRUSH_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;

        Ok(vec![PathBuf::from(CRUSH_JSON_FILE)])
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
        CRUSH_JSON_FILE, CrushAdapter, SUPPORTED_HOOK_EVENTS, overlay_native_crush,
        render_permission_rule,
    };
    use crate::adapter::AgentAdapter;
    use crate::config::{
        Capabilities, Hook, HookHandler, HookHandlerKind, NativePermissionRules, PermissionRule,
    };
    use crate::merge::MergedManifest;

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
}
