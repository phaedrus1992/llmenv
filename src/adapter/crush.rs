use std::path::{Path, PathBuf};

use serde_json::json;

use super::AgentAdapter;
use super::resolve_bundle_relative_paths;
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

    fn supports_model_providers(&self) -> bool {
        true
    }

    fn supported_hook_events(&self) -> &'static [&'static str] {
        SUPPORTED_HOOK_EVENTS
    }

    fn env_vars(
        &self,
        cache_dir: &Path,
        state_dir: &Path,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let config_dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        let data_dir = state_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("state_dir is not valid UTF-8: {}", state_dir.display())
        })?;
        // ponytail: creating a crush-specific subdir in state_dir to isolate Crush's runtime
        // data from other tools' state dirs. Allows future Crush-specific state cleanup
        // without touching unrelated stores.
        let crush_data_dir = format!("{data_dir}/crush");
        // ponytail: env_vars() does I/O here (breaking the "query-only" trait shape)
        // because it's the only place that knows both the exact path and that it
        // must exist — nothing else in the export pipeline creates this adapter's
        // state subdir. Single call site today (cli/mod.rs run_export). If a second
        // caller needs env_vars() without the mkdir side effect (e.g. a dry-run
        // command), split dir creation into materialize() and thread state_dir
        // through its signature instead.
        super::skills::create_dir_owner_only(Path::new(&crush_data_dir))?;
        // Crush's `GlobalConfig()` does `filepath.Join(CRUSH_GLOBAL_CONFIG, "crush.json")`
        // itself — this must be the directory containing crush.json, not the file path,
        // or Crush ends up joining "crush.json" onto an already-file-ending path.
        Ok(vec![
            ("CRUSH_GLOBAL_CONFIG".into(), config_dir.to_string()),
            ("CRUSH_GLOBAL_DATA".into(), crush_data_dir),
        ])
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        // 1. Create output dir with owner-only permissions
        super::skills::create_dir_owner_only(out)?;

        // 2. Filter hooks Crush can't express (#543 follow-up): a bundle shared
        // across engines commonly declares hooks only Claude Code supports (e.g.
        // PostToolUse). That is a cross-engine compatibility gap, not a
        // config-authoring mistake — failing the whole render over one
        // incompatible hook would also drop every other capability (MCP, LSP,
        // skills, permissions) Crush *can* express. Skip the incompatible hook
        // and warn loudly instead so the rest of the config still materializes.
        let compatible_hooks: Vec<&crate::config::Hook> = manifest
            .capabilities
            .hooks
            .iter()
            .filter(|hook| {
                if !SUPPORTED_HOOK_EVENTS.contains(&hook.event.as_str()) {
                    eprintln!(
                        "warning: Crush adapter does not support hook event '{}' — \
                         skipping this hook. Supported events: {}. Remove or move \
                         this hook to a claude_code-only bundle to silence this warning.",
                        hook.event,
                        SUPPORTED_HOOK_EVENTS.join(", ")
                    );
                    return false;
                }
                if matches!(hook.handler.kind, crate::config::HookHandlerKind::McpTool) {
                    eprintln!(
                        "warning: Crush adapter does not support mcp_tool hooks \
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

        // 3. Write first-class skills (fix 2)
        let skill_paths =
            crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;

        // 4. Project plugin skills, skipping plugins with non-skill content Crush
        // can't express (#543 follow-up: was a hard-error that aborted the whole
        // render over one incompatible plugin, dropping every other plugin's
        // skills, MCP servers, permissions, and hooks along with it).
        let mut owned: Vec<PathBuf> = vec![PathBuf::from(CRUSH_JSON_FILE)];
        owned.extend(skill_paths.iter().cloned());

        let mut plugin_skill_paths: Vec<PathBuf> = Vec::new();
        'plugin: for plugin in &manifest.plugins {
            let payload = resolve_plugin_payload(plugin, &manifest.marketplaces)?;
            for bad_dir in &["agents", "commands", "hooks"] {
                if payload.join(bad_dir).is_dir() {
                    eprintln!(
                        "warning: plugin '{}' contains unsupported Crush content: '{}/' \
                         directory — skipping this plugin. Crush has no equivalent for \
                         plugin agents, commands, or hooks. Scope this bundle away from \
                         Crush with `when:` or remove the content to silence this warning.",
                        plugin.plugin, bad_dir
                    );
                    continue 'plugin;
                }
            }
            let paths = crate::adapter::skills::project_plugin_skills(&payload, out)?;
            plugin_skill_paths.extend(paths);
        }
        owned.extend(plugin_skill_paths.iter().cloned());

        // Built-in `llmenv` skill: one reference file per enabled first-party
        // feature. No-op when none are enabled. Counted toward `skills_paths`
        // below so Crush discovers it even when it's the only skill present.
        let features = manifest.capabilities.features.clone().unwrap_or_default();
        let llmenv_skill_paths =
            crate::adapter::llmenv_skill::materialize_llmenv_skill(out, &features)?;
        owned.extend(llmenv_skill_paths.iter().cloned());

        // P1-1: validate skills (frontmatter + hardcoded-path scan), same gate as ClaudeCodeAdapter
        crate::adapter::skills::validate_skills(out)?;

        // 5. Build doc
        let mut doc = serde_json::Map::new();

        // Hooks: Crush's HookConfig (internal/config/config.go) is a flat
        // { matcher?, command, name?, timeout? } object per event entry — unlike
        // Claude Code's { matcher, hooks: [{ type, command, tool }] } nesting.
        // Rendering the nested shape here means Crush reads an empty `command`
        // off the wrapper object and rejects the whole config at load time.
        let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
            std::collections::BTreeMap::new();
        for hook in &compatible_hooks {
            // P2-6: mcp_tool hooks are filtered out at the gate above, so `command`
            // is always present for the remaining Command-kind hooks.
            let resolved_command =
                hook.handler
                    .command
                    .as_ref()
                    .map(|cmd| match &hook.bundle_origin {
                        Some(bundle_dir) => resolve_bundle_relative_paths(cmd, bundle_dir)
                            .unwrap_or_else(|| cmd.clone()),
                        None => cmd.clone(),
                    });
            let mut entry = serde_json::Map::new();
            if let Some(matcher) = &hook.matcher {
                entry.insert("matcher".into(), json!(matcher));
            }
            // Omit `command` when absent rather than emitting `"command": null`
            // — a null-valued key violates the no-null invariant and Crush
            // rejects a wrapper carrying an empty command anyway (#720).
            if let Some(command) = &resolved_command {
                entry.insert("command".into(), json!(command));
            }
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

        // Permissions: Crush's PermissionsConfig (internal/config/config.go) has
        // exactly one field, `allowed_tools` — an allow-list of tools that skip
        // the interactive approval prompt. There is no `denied_tools` or
        // `default_mode` concept: any tool not in the allow-list already
        // requires prompt approval by default (deny-by-default), so `ask` and
        // `deny` rules need no explicit rendering — omitting a tool from
        // `allowed_tools` already produces the fail-closed behavior. Rendering
        // extra keys here previously did nothing (Crush's plain
        // `json.Unmarshal` silently drops unknown fields), so this was already
        // a no-op, not a security regression — just dead output (#554).
        let perms = &manifest.capabilities.permissions;
        let native_perms = manifest.capabilities.native_permissions.get("crush");

        let mut allowed_tools = render_rules_to_strings(&perms.allow);
        if let Some(n) = native_perms {
            allowed_tools.extend(n.allow.iter().cloned());
        }
        dedup(&mut allowed_tools);

        if !allowed_tools.is_empty() {
            let mut perm_obj = serde_json::Map::new();
            perm_obj.insert("allowed_tools".into(), json!(allowed_tools));
            doc.insert("permissions".into(), serde_json::Value::Object(perm_obj));
        }

        // MCP servers (fix 6: headers/timeout/disabled_tools)
        //
        // Crush's MCPConfig.Type (internal/config/config.go) is a *required* field
        // with exactly three valid values: "stdio", "sse", "http". Its MCP client
        // dispatches on this field (internal/agent/tools/mcp/init.go) and returns
        // "unsupported mcp type" for anything else, including a missing/empty
        // value — so every server previously failed to initialize: stdio entries
        // carried no `type` at all, and remote entries carried the invalid
        // literal `"remote"`.
        if !manifest.mcps.is_empty() || manifest.capabilities.native_mcp.contains_key("crush") {
            let mut mcp_obj = serde_json::Map::new();
            for mcp in &manifest.mcps {
                use crate::mcp::resolve::ResolvedKind;
                let mut e = match &mcp.kind {
                    ResolvedKind::Stdio { command, args, env } => {
                        let mut e = serde_json::Map::new();
                        e.insert("type".into(), json!("stdio"));
                        e.insert("command".into(), json!(command));
                        e.insert("args".into(), json!(args));
                        if !env.is_empty() {
                            e.insert("env".into(), json!(env));
                        }
                        e
                    }
                    ResolvedKind::Remote { url, transport } => {
                        let mut e = serde_json::Map::new();
                        e.insert(
                            "type".into(),
                            json!(super::remote_transport_type_str(*transport)),
                        );
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

        // Model providers (fix 1 pattern): skip disabled providers; omit
        // "providers" key if none remain. The JSON tags here match catwalk's
        // Provider/Model struct tags (confirmed in Task 5 of the spec).
        if !manifest.capabilities.model_providers.is_empty() {
            let providers_value = render_model_providers(&manifest.capabilities.model_providers)?;
            if providers_value.as_object().is_some_and(|o| !o.is_empty()) {
                doc.insert("providers".into(), providers_value);
            }
        }

        // Default models (fix 1 pattern): omit "models" key if none.
        if !manifest.capabilities.default_models.is_empty() {
            let models_value = render_default_models(&manifest.capabilities.default_models);
            if models_value.as_object().is_some_and(|o| !o.is_empty()) {
                doc.insert("models".into(), models_value);
            }
        }

        // options.skills_paths: emit whenever any skills exist (first-class or plugin-projected).
        // P1-2: must include plugin_skill_paths — plugin-only skill sets omit this key otherwise.
        if !skill_paths.is_empty()
            || !plugin_skill_paths.is_empty()
            || !llmenv_skill_paths.is_empty()
        {
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
    // P2-5/#534: guard before any path join, regardless of which path is taken.
    if !crate::paths::is_valid_short_name(&plugin.plugin) {
        anyhow::bail!("plugin name '{}' is not a valid name", plugin.plugin);
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
                    "LSP server '{}': failed to convert init_options to JSON: {err}",
                    srv.name
                )
            })?;
            // Crush's LSPConfig field is `init_options` (snake_case) — not
            // Claude Code's `initializationOptions`.
            e.insert("init_options".into(), as_json);
        }
        lsp_obj.insert(srv.name.clone(), serde_json::Value::Object(e));
    }
    Ok(serde_json::Value::Object(lsp_obj))
}

/// Build the `providers` JSON object (keyed by provider id) from a slice of model providers.
///
/// Disabled providers (`disabled == true`) are skipped entirely. The JSON tags match
/// catwalk's Provider/Model struct tags (confirmed in Task 5 of the spec).
fn render_model_providers(
    providers: &[llmenv_config::ModelProvider],
) -> anyhow::Result<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for p in providers {
        if p.disabled {
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), json!(p.id));
        if let Some(name) = &p.name {
            entry.insert("name".into(), json!(name));
        }
        if let Some(base_url) = &p.base_url {
            entry.insert("api_endpoint".into(), json!(base_url));
        }
        if let Some(api_type) = &p.api_type {
            entry.insert("type".into(), json!(api_type));
        }
        if let Some(api_key) = &p.api_key {
            entry.insert("api_key".into(), json!(api_key));
        }
        if !p.headers.is_empty() {
            entry.insert("default_headers".into(), json!(p.headers));
        }
        if !p.models.is_empty() {
            let models: Vec<serde_json::Value> = p.models.iter().map(render_model_source).collect();
            entry.insert("models".into(), json!(models));
        }
        obj.insert(p.id.clone(), serde_json::Value::Object(entry));
    }
    Ok(serde_json::Value::Object(obj))
}

/// Render a single model source as a JSON object matching catwalk's Model struct.
///
/// catwalk.Model field-name mapping (confirmed Task 5):
///   ModelSource.id            → "id"
///   ModelSource.name          → "name"           (optional)
///   ModelSource.reasoning     → "can_reason"     (if true)
///   ModelSource.context_window → "context_window" (optional)
///   ModelSource.max_tokens    → "default_max_tokens" (optional)
///   ModelSource.cost.input    → "cost_per_1m_in"
///   ModelSource.cost.output   → "cost_per_1m_out"
///   ModelSource.cost.cache_read  → "cost_per_1m_in_cached"  (optional)
///   ModelSource.cost.cache_write → "cost_per_1m_out_cached" (optional)
///
/// Cost fields are flat on the Model struct (not nested under "cost"), matching
/// catwalk's `CostPer1MIn` / `CostPer1MOut` / `CostPer1MInCached` / `CostPer1MOutCached`.
fn render_model_source(m: &llmenv_config::ModelSource) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert("id".into(), json!(m.id));
    if let Some(name) = &m.name {
        entry.insert("name".into(), json!(name));
    }
    if m.reasoning {
        entry.insert("can_reason".into(), json!(true));
    }
    if let Some(ctx) = m.context_window {
        entry.insert("context_window".into(), json!(ctx));
    }
    if let Some(max) = m.max_tokens {
        entry.insert("default_max_tokens".into(), json!(max));
    }
    if let Some(cost) = &m.cost {
        entry.insert("cost_per_1m_in".into(), json!(cost.input));
        entry.insert("cost_per_1m_out".into(), json!(cost.output));
        if let Some(cr) = cost.cache_read {
            entry.insert("cost_per_1m_in_cached".into(), json!(cr));
        }
        if let Some(cw) = cost.cache_write {
            entry.insert("cost_per_1m_out_cached".into(), json!(cw));
        }
    }
    serde_json::Value::Object(entry)
}

/// Build the `models` JSON object (keyed by scope role) for per-scope default model selection.
///
/// Each value is `{"provider": "<id>", "model": "<model-id>"}` matching the shape
/// consumed by Crush for default-model routing.
fn render_default_models(
    models: &std::collections::BTreeMap<String, llmenv_config::ModelRef>,
) -> serde_json::Value {
    let obj: serde_json::Map<String, serde_json::Value> = models
        .iter()
        .map(|(role, r#ref)| {
            (
                role.clone(),
                json!({ "provider": r#ref.provider, "model": r#ref.model }),
            )
        })
        .collect();
    serde_json::Value::Object(obj)
}

/// Keys that are fully modeled by CrushAdapter and must not appear in the `native.crush`
/// catch-all fragment. Overlaying them last would silently clobber the security-rendered
/// output (permissions, hooks) or the structured rendering (mcp, lsp, providers, models).
///
/// Use the dedicated `native_permissions.crush` / `native_hooks.crush` / `native_mcp.crush`
/// channels instead, which merge in the safe direction.
const CRUSH_MODELED_KEYS: &[&str] = &["permissions", "hooks", "mcp", "lsp", "providers", "models"];

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

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::{
        CRUSH_JSON_FILE, CRUSH_MODELED_KEYS, CrushAdapter, SUPPORTED_HOOK_EVENTS,
        overlay_native_crush, reject_modeled_keys_in_native_crush, render_permission_rule,
    };
    use crate::adapter::AgentAdapter;
    use crate::adapter::skills::arb_yaml_value;
    use crate::config::{
        Capabilities, Hook, HookHandler, HookHandlerKind, NativePermissionRules, PermissionRule,
    };
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::merge::MergedManifest;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn empty_manifest() -> MergedManifest {
        MergedManifest::default()
    }

    fn manifest_with_caps(caps: Capabilities) -> MergedManifest {
        MergedManifest {
            capabilities: caps,
            ..Default::default()
        }
    }

    #[test]
    fn materialize_llmenv_skill_when_task_tracker_enabled() {
        let out = tempfile::tempdir().unwrap();
        let caps = Capabilities {
            features: Some(crate::config::Features {
                task_tracker: Some(crate::config::TaskTracker { enabled: true }),
                ..Default::default()
            }),
            ..Default::default()
        };
        CrushAdapter
            .materialize(&manifest_with_caps(caps), out.path())
            .unwrap();
        assert!(out.path().join("skills/llmenv/SKILL.md").exists());
        assert!(
            out.path()
                .join("skills/llmenv/references/task-tracker.md")
                .exists()
        );
    }

    #[test]
    fn no_llmenv_skill_when_no_features_enabled() {
        let out = tempfile::tempdir().unwrap();
        CrushAdapter
            .materialize(&empty_manifest(), out.path())
            .unwrap();
        assert!(!out.path().join("skills/llmenv").exists());
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
            mcp_permissions: None,
        }
    }

    // ── env_vars ──────────────────────────────────────────────────────────────

    #[test]
    fn env_vars_returns_config_and_data() {
        let cache = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(cache.path(), state.path()).unwrap();
        assert_eq!(vars.len(), 2);
        assert!(vars.iter().any(|(k, _)| k == "CRUSH_GLOBAL_CONFIG"));
        assert!(vars.iter().any(|(k, _)| k == "CRUSH_GLOBAL_DATA"));
    }

    #[test]
    fn env_vars_config_path_is_the_cache_dir_not_the_json_file() {
        // Crush's GlobalConfig() does filepath.Join(CRUSH_GLOBAL_CONFIG, "crush.json")
        // itself. If we point this var at the crush.json file path, Crush ends up
        // looking for crush.json/crush.json (#regression).
        let cache = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(cache.path(), state.path()).unwrap();
        let (_, config) = vars
            .iter()
            .find(|(k, _)| k == "CRUSH_GLOBAL_CONFIG")
            .unwrap();
        assert_eq!(
            config,
            &cache.path().to_str().unwrap().to_string(),
            "CRUSH_GLOBAL_CONFIG must be the cache dir itself, not the crush.json file"
        );
    }

    #[test]
    fn env_vars_data_dir_is_state_subdir() {
        let cache = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(cache.path(), state.path()).unwrap();
        let (_, data) = vars.iter().find(|(k, _)| k == "CRUSH_GLOBAL_DATA").unwrap();
        let expected = format!("{}/crush", state.path().display());
        assert_eq!(
            data, &expected,
            "CRUSH_GLOBAL_DATA should point to <state_dir>/crush"
        );
    }

    #[test]
    fn env_vars_creates_data_dir_on_disk() {
        let cache = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let vars = CrushAdapter.env_vars(cache.path(), state.path()).unwrap();
        let (_, data) = vars.iter().find(|(k, _)| k == "CRUSH_GLOBAL_DATA").unwrap();
        assert!(
            std::path::Path::new(data).is_dir(),
            "CRUSH_GLOBAL_DATA dir '{data}' must exist on disk after env_vars() runs"
        );
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
    fn materialize_command_hook_without_command_omits_null_key() {
        // A Command-kind hook with no command string must not render
        // `"command": null` — a null-valued key violates the no-null invariant
        // (#720) and Crush rejects a wrapper carrying an empty command anyway.
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(Hook {
            event: "PreToolUse".into(),
            matcher: Some("Bash".into()),
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: None,
                tool: None,
            },
            bundle_origin: None,
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = &doc["hooks"]["PreToolUse"][0];
        assert!(
            !entry.as_object().unwrap().contains_key("command"),
            "absent command must be omitted, not rendered as null: {entry}"
        );
    }

    #[test]
    fn materialize_hook_uses_crush_flat_shape_not_claude_nesting() {
        // Crush's HookConfig (internal/config/config.go) is a flat
        // { matcher?, command } object per event entry, not Claude Code's
        // { matcher, hooks: [{ type, command, tool }] } nesting. Rendering the
        // nested shape makes Crush read an empty `command` off the wrapper and
        // reject the config with "command is required" (#551 follow-up).
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(Hook {
            event: "PreToolUse".into(),
            matcher: Some("Bash".into()),
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("echo hi".into()),
                tool: None,
            },
            bundle_origin: None,
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = &doc["hooks"]["PreToolUse"][0];
        assert_eq!(entry["command"], serde_json::json!("echo hi"));
        assert_eq!(entry["matcher"], serde_json::json!("Bash"));
        assert!(
            entry.get("hooks").is_none(),
            "must not nest under a Claude Code-style 'hooks' array: {entry}"
        );
        assert!(
            entry.get("type").is_none(),
            "Crush's HookConfig has no 'type' field: {entry}"
        );
        assert!(
            entry.get("tool").is_none(),
            "Crush's HookConfig has no 'tool' field: {entry}"
        );
    }

    #[test]
    fn materialize_hook_resolves_bundle_relative_command_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(Hook {
            event: "PreToolUse".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("bash hooks/guard.sh".into()),
                tool: None,
            },
            bundle_origin: Some(PathBuf::from("/bundles/mybundle")),
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["command"],
            serde_json::json!("bash /bundles/mybundle/hooks/guard.sh"),
            "bundle-relative hook script path must resolve to an absolute path: {doc}"
        );
    }

    #[test]
    fn materialize_unsupported_hook_event_is_skipped_not_fatal() {
        // #543 follow-up: an incompatible hook must not fail the whole render —
        // it's a cross-engine compatibility gap (a bundle shared with Claude
        // Code), not a config-authoring mistake. Skip it (with a warning) and
        // still materialize everything Crush can express.
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
        let owned = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .expect("unsupported hook must not fail materialize");
        assert!(owned.contains(&PathBuf::from(CRUSH_JSON_FILE)));
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("hooks").is_none(),
            "unsupported hook must not appear in output: {doc}"
        );
    }

    #[test]
    fn materialize_mixed_supported_and_unsupported_hooks_keeps_supported() {
        // The concrete regression this guards: a bundle with both a Crush-compatible
        // hook and an incompatible one must still render the compatible one, not
        // drop everything because one hook couldn't be expressed.
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(pretooluse_hook("echo hi"));
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
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .expect("one incompatible hook must not fail the whole render");
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc["hooks"]["PreToolUse"].is_array(),
            "supported hook must still render: {doc}"
        );
        assert!(
            doc["hooks"].get("PostToolUse").is_none(),
            "unsupported hook must not appear in output: {doc}"
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
    fn materialize_ask_and_deny_rules_produce_no_permissions_output() {
        // Crush's PermissionsConfig has only `allowed_tools` (no `denied_tools` /
        // `default_mode` concept — see internal/config/config.go). Anything not
        // in the allow-list already requires interactive approval by default, so
        // `ask`/`deny` rules correctly produce no permissions output at all
        // rather than an unknown key Crush would silently ignore (#554).
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.ask.push(PermissionRule {
            tool: "WebFetch".into(),
            pattern: None,
            paths: vec![],
        });
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
        assert!(
            doc.get("permissions").is_none(),
            "ask/deny-only config must produce no permissions key: {doc}"
        );
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
    fn materialize_native_permissions_ask_produces_no_permissions_output() {
        // Same rationale as materialize_ask_and_deny_rules_produce_no_permissions_output,
        // for the native_permissions.crush channel.
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
        assert!(
            doc.get("permissions").is_none(),
            "native ask-only config must produce no permissions key: {doc}"
        );
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

    #[test]
    fn materialize_full_config_matches_charm_land_crush_schema_shape() {
        // Regression test for #554: every field name/shape here was checked
        // against the real schema at https://charm.land/crush.json (mirrored
        // from internal/config/config.go in Crush's own source) — MCPConfig's
        // required `type` enum (stdio/sse/http), LSPConfig's `init_options`
        // (not Claude Code's `initializationOptions`), the flat HookConfig
        // shape, and PermissionsConfig's `allowed_tools`-only surface.
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.hooks.push(pretooluse_hook("echo hi"));
        caps.hooks.push(Hook {
            event: "PreToolUse".into(),
            matcher: Some("Bash".into()),
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("bash hooks/guard.sh".into()),
                tool: None,
            },
            bundle_origin: Some(PathBuf::from("/bundles/foo")),
        });
        caps.permissions.allow.push(PermissionRule {
            tool: "Bash".into(),
            pattern: Some("ls*".into()),
            paths: vec![],
        });
        caps.permissions.ask.push(PermissionRule {
            tool: "WebFetch".into(),
            pattern: None,
            paths: vec![],
        });
        caps.lsp.push(llmenv_config::LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            args: vec!["--quiet".into()],
            filetypes: vec!["rust".into()],
            root_markers: vec!["Cargo.toml".into()],
            timeout: Some(60),
            init_options: Some(serde_yaml::from_str("checkOnSave: true").unwrap()),
            ..Default::default()
        });
        let skill_src = tempfile::tempdir().unwrap();
        std::fs::write(
            skill_src.path().join("SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill.\n---\n# MySkill\n",
        )
        .unwrap();
        caps.skills.push(crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.path().to_string_lossy().into_owned(),
            when: Vec::new(),
        });

        let mut manifest = manifest_with_caps(caps);
        manifest.mcps.push(stdio_mcp("stdio-server"));
        manifest.mcps.push(ResolvedMcp {
            name: "http-server".into(),
            kind: ResolvedKind::Remote {
                url: "http://localhost:3000/mcp".into(),
                transport: crate::config::McpTransport::Http,
            },
            headers: std::collections::BTreeMap::from([(
                "Authorization".into(),
                "Bearer tok".into(),
            )]),
            timeout: Some(30),
            disabled_tools: vec!["dangerous_tool".into()],
            mcp_permissions: None,
        });
        manifest.mcps.push(ResolvedMcp {
            name: "sse-server".into(),
            kind: ResolvedKind::Remote {
                url: "http://localhost:4000/sse".into(),
                transport: crate::config::McpTransport::Sse,
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
        });

        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // hooks: flat HookConfig, no Claude Code-style nesting.
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["command"],
            serde_json::json!("echo hi")
        );
        assert!(doc["hooks"]["PreToolUse"][0].get("hooks").is_none());

        // mcp: every transport carries the schema's required `type` enum value.
        assert_eq!(
            doc["mcp"]["stdio-server"]["type"],
            serde_json::json!("stdio")
        );
        assert_eq!(doc["mcp"]["http-server"]["type"], serde_json::json!("http"));
        assert_eq!(doc["mcp"]["sse-server"]["type"], serde_json::json!("sse"));

        // lsp: init_options (snake_case), not initializationOptions.
        assert_eq!(
            doc["lsp"]["rust-analyzer"]["init_options"]["checkOnSave"],
            serde_json::json!(true)
        );
        assert!(
            doc["lsp"]["rust-analyzer"]
                .get("initializationOptions")
                .is_none()
        );

        // permissions: allow-only surface; ask rule produces no denied_tools key.
        assert_eq!(
            doc["permissions"]["allowed_tools"],
            serde_json::json!(["Bash(ls*)"])
        );
        assert!(doc["permissions"].get("denied_tools").is_none());
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
            srv.get("init_options").is_none(),
            "None init_options must be omitted"
        );
    }

    #[test]
    fn materialize_lsp_init_options_uses_crush_snake_case_key() {
        // Crush's LSPConfig field is `init_options` (snake_case), not Claude
        // Code's `initializationOptions` — using the wrong key means Crush's
        // plain json.Unmarshal silently drops the value (#554).
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.lsp.push(llmenv_config::LspServer {
            name: "gopls".into(),
            command: "gopls".into(),
            init_options: Some(serde_yaml::from_str("usePlaceholders: true").unwrap()),
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["lsp"]["gopls"]["init_options"]["usePlaceholders"],
            serde_json::json!(true)
        );
        assert!(
            doc["lsp"]["gopls"].get("initializationOptions").is_none(),
            "must not use Claude Code's camelCase key"
        );
    }

    // ── materialize: mcp_tool hook is skipped, not fatal (#543 follow-up) ────

    #[test]
    fn materialize_mcp_tool_hook_is_skipped_not_fatal() {
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
        let owned = CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .expect("mcp_tool hook must not fail materialize");
        assert!(owned.contains(&PathBuf::from(CRUSH_JSON_FILE)));
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("hooks").is_none(),
            "mcp_tool hook must not appear in output: {doc}"
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

    // ── materialize: model providers (fix 1 pattern) ──────────────────────

    #[test]
    fn materialize_model_provider_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.model_providers.push(llmenv_config::ModelProvider {
            id: "ollama".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            api_type: Some("openai".into()),
            models: vec![llmenv_config::ModelSource {
                id: "llama3.1:8b".into(),
                context_window: Some(128_000),
                ..Default::default()
            }],
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Provider-level fields use catwalk's JSON tags (confirmed Task 5)
        assert_eq!(
            doc["providers"]["ollama"]["api_endpoint"],
            serde_json::json!("http://localhost:11434/v1"),
            "provider api_endpoint must be written"
        );
        assert_eq!(
            doc["providers"]["ollama"]["type"],
            serde_json::json!("openai"),
            "provider type must be written"
        );
        // Model-level fields use catwalk's Model struct tags
        assert_eq!(
            doc["providers"]["ollama"]["models"][0]["id"],
            serde_json::json!("llama3.1:8b"),
            "model id must be written"
        );
        assert_eq!(
            doc["providers"]["ollama"]["models"][0]["context_window"],
            serde_json::json!(128_000),
            "model context_window must be written"
        );
    }

    #[test]
    fn materialize_model_provider_disabled_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.model_providers.push(llmenv_config::ModelProvider {
            id: "disabled-provider".into(),
            disabled: true,
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("providers").is_none(),
            "\"providers\" key must be absent when all providers are disabled"
        );
    }

    #[test]
    fn materialize_model_provider_empty_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        CrushAdapter
            .materialize(&empty_manifest(), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("providers").is_none(),
            "\"providers\" key must be absent when no model providers configured"
        );
    }

    #[test]
    fn materialize_model_source_optional_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.model_providers.push(llmenv_config::ModelProvider {
            id: "test".into(),
            name: Some("Test Provider".into()),
            api_key: Some("sk-test".into()),
            models: vec![llmenv_config::ModelSource {
                id: "test-model".into(),
                name: Some("Test Model".into()),
                reasoning: true,
                context_window: Some(128_000),
                max_tokens: Some(16_384),
                cost: Some(llmenv_config::ModelCost {
                    input: 0.15,
                    output: 0.60,
                    cache_read: Some(0.075),
                    cache_write: Some(0.15),
                }),
                ..Default::default()
            }],
            ..Default::default()
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let model = &doc["providers"]["test"]["models"][0];

        assert_eq!(model["name"], serde_json::json!("Test Model"));
        assert_eq!(model["can_reason"], serde_json::json!(true));
        assert_eq!(model["default_max_tokens"], serde_json::json!(16_384));
        // Cost fields are flat on the model, not nested under "cost"
        assert_eq!(model["cost_per_1m_in"], serde_json::json!(0.15));
        assert_eq!(model["cost_per_1m_out"], serde_json::json!(0.60));
        assert_eq!(model["cost_per_1m_in_cached"], serde_json::json!(0.075));
        assert_eq!(model["cost_per_1m_out_cached"], serde_json::json!(0.15));
    }

    #[test]
    fn materialize_default_model_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.default_models.insert(
            "large".into(),
            llmenv_config::ModelRef {
                provider: "anthropic".into(),
                model: "claude-opus-4-7".into(),
            },
        );
        caps.default_models.insert(
            "small".into(),
            llmenv_config::ModelRef {
                provider: "anthropic".into(),
                model: "claude-haiku-4-5".into(),
            },
        );
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["models"]["large"]["provider"],
            serde_json::json!("anthropic")
        );
        assert_eq!(
            doc["models"]["large"]["model"],
            serde_json::json!("claude-opus-4-7")
        );
        assert_eq!(
            doc["models"]["small"]["provider"],
            serde_json::json!("anthropic")
        );
        assert_eq!(
            doc["models"]["small"]["model"],
            serde_json::json!("claude-haiku-4-5")
        );
    }

    #[test]
    fn materialize_default_model_empty_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        CrushAdapter
            .materialize(&empty_manifest(), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("models").is_none(),
            "\"models\" key must be absent when no default models configured"
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
    fn materialize_plugin_with_agents_is_skipped_not_fatal() {
        // #543 follow-up: an incompatible plugin must not fail the whole render —
        // it would drop every other plugin's skills, MCP servers, permissions,
        // and hooks along with it. Skip just this plugin (with a warning).
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
        CrushAdapter
            .materialize(&manifest, tmp.path())
            .expect("incompatible plugin content must not fail materialize");
    }

    #[test]
    fn materialize_plugin_with_hooks_dir_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        // Plugin dir with a hooks/ subdirectory — Crush has no plugin-hooks
        // equivalent, so this plugin is skipped, but the rest of the config
        // (other plugins, permissions, MCP, compatible hooks) still renders.
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
        CrushAdapter
            .materialize(&manifest, tmp.path())
            .expect("incompatible plugin content must not fail materialize");
    }

    #[test]
    fn materialize_plugin_with_hooks_dir_keeps_other_plugin_skills() {
        // The concrete regression this guards: one plugin with unsupported
        // content must not prevent an unrelated, compatible plugin's skills
        // from being projected.
        let tmp = tempfile::tempdir().unwrap();
        let bad_plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(bad_plugin_dir.path().join("hooks")).unwrap();

        let good_plugin_dir = tempfile::tempdir().unwrap();
        let skill_dir = good_plugin_dir.path().join("skills/foo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: foo\ndescription: a foo skill\n---\nBody",
        )
        .unwrap();

        let mut manifest = empty_manifest();
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "hooky-plugin".into(),
                collection: String::new(),
                install_path: Some(bad_plugin_dir.path().to_string_lossy().into_owned()),
                git_commit_sha: None,
            });
        manifest
            .plugins
            .push(crate::plugins::resolve::ResolvedPlugin {
                marketplace: "local".into(),
                plugin: "good-plugin".into(),
                collection: String::new(),
                install_path: Some(good_plugin_dir.path().to_string_lossy().into_owned()),
                git_commit_sha: None,
            });
        CrushAdapter
            .materialize(&manifest, tmp.path())
            .expect("one incompatible plugin must not fail the whole render");
        assert!(
            tmp.path().join("skills/foo/SKILL.md").exists(),
            "unrelated plugin's skill must still be projected"
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
            err.to_string().contains("not a valid name"),
            "error must reject the invalid plugin name: {err}"
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

        // ── P2: render_permission_rule ────────────────────────────────────────

        #[test]
        fn prop_render_permission_rule_pattern_wins_over_paths(
            tool in "[A-Za-z]{1,15}",
            pattern in "[a-z*]{1,15}",
            paths in prop::collection::vec("[a-z/]{1,15}", 0..5),
        ) {
            let rule = crate::config::PermissionRule {
                tool: tool.clone(),
                pattern: Some(pattern.clone()),
                paths,
            };
            // When `pattern` is Some, `paths` is ignored — output is exactly one
            // entry built from tool+pattern, regardless of how many paths exist.
            prop_assert_eq!(
                render_permission_rule(&rule),
                vec![format!("{tool}({pattern})")]
            );
        }

        #[test]
        fn prop_render_permission_rule_no_panic(
            tool in ".*",
            pattern in prop::option::of(".*"),
            paths in prop::collection::vec(".*", 0..5),
        ) {
            let rule = crate::config::PermissionRule { tool, pattern, paths };
            let _ = render_permission_rule(&rule);
        }

        // ── P2: overlay_native_crush ─────────────────────────────────────────

        #[test]
        fn prop_overlay_native_crush_idempotent(
            fragment in prop::collection::hash_map("[a-z]{1,8}", 0i64..1000, 0..5),
        ) {
            let frag_yaml: serde_yaml::Value = serde_yaml::to_value(&fragment).unwrap();

            let mut once = serde_json::json!({});
            overlay_native_crush(&mut once, Some(&frag_yaml)).unwrap();

            let mut twice = serde_json::json!({});
            overlay_native_crush(&mut twice, Some(&frag_yaml)).unwrap();
            overlay_native_crush(&mut twice, Some(&frag_yaml)).unwrap();

            prop_assert_eq!(once, twice, "applying the same fragment twice must equal applying it once");
        }

        #[test]
        fn prop_overlay_native_crush_no_panic(
            fragment in arb_yaml_value(3),
        ) {
            let mut dst = serde_json::json!({"existing": "value"});
            let _ = overlay_native_crush(&mut dst, Some(&fragment));
        }

        // ── model_providers ──────────────────────────────────────────────────

        #[test]
        fn prop_render_model_providers_keys_match_non_disabled(
            ids in prop::collection::vec("[a-z][a-z0-9-]{0,15}", 0..6),
            disabled_flags in prop::collection::vec(proptest::bool::ANY, 0..6),
        ) {
            let providers: Vec<llmenv_config::ModelProvider> = ids
                .iter()
                .zip(disabled_flags.iter())
                .map(|(id, &d)| llmenv_config::ModelProvider {
                    id: id.clone(),
                    disabled: d,
                    ..Default::default()
                })
                .collect();
            let expected: std::collections::BTreeSet<String> = providers
                .iter()
                .filter(|p| !p.disabled)
                .map(|p| p.id.clone())
                .collect();
            let result = super::render_model_providers(&providers).unwrap();
            let got: std::collections::BTreeSet<String> = result
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            prop_assert_eq!(got, expected);
        }

        #[test]
        fn prop_render_model_providers_no_panic(
            id in ".*",
            base_url in prop::option::of(".*"),
            api_key in prop::option::of(".*"),
        ) {
            let provider = llmenv_config::ModelProvider {
                id,
                base_url,
                api_key,
                ..Default::default()
            };
            let _ = super::render_model_providers(std::slice::from_ref(&provider));
        }

        #[test]
        fn prop_render_default_models_no_panic(
            role in ".*",
            provider in ".*",
            model in ".*",
        ) {
            let mut map = std::collections::BTreeMap::new();
            map.insert(role, llmenv_config::ModelRef { provider, model });
            let _ = super::render_default_models(&map);
        }
    }
}
