use std::path::{Path, PathBuf};

use serde_json::json;

use super::AgentAdapter;
use super::resolve_bundle_relative_paths;
use crate::merge::MergedManifest;
use crate::util::dedup;

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

    fn is_active(&self) -> bool {
        std::env::var("CRUSH_GLOBAL_CONFIG").is_ok()
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

    /// Every map this adapter reads — `native_plugins` is absent because Crush
    /// has no Claude-style plugin concept.
    fn native_maps(&self) -> &'static [&'static str] {
        use crate::adapter::native_keys as nk;
        &[
            nk::NATIVE_PERMISSIONS,
            nk::NATIVE_HOOKS,
            nk::NATIVE_MCP,
            nk::NATIVE_MODEL_PROVIDERS,
            nk::NATIVE_DEFAULT_MODELS,
            nk::NATIVE,
        ]
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
            let payload = super::resolve_plugin_payload(plugin, &manifest.marketplaces)?;
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
                            .unwrap_or_else(|| {
                                tracing::warn!(
                                    "failed to resolve bundle-relative path for command in {:?}: {cmd:?}",
                                    bundle_dir
                                );
                                cmd.clone()
                            }),
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
        super::overlay_native_json(
            &mut hooks_value,
            manifest.capabilities.native_hooks.get("crush"),
            "native_hooks.crush",
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
        // `default_mode` concept, so `ask`/`deny` rules render no key of their
        // own — omitting a tool from `allowed_tools` is already fail-closed.
        // Rendering extra keys here previously did nothing (Crush's plain
        // `json.Unmarshal` silently drops unknown fields), so this was already
        // a no-op, not a security regression — just dead output (#554).
        let perms = &manifest.capabilities.permissions;
        let native_perms = manifest.capabilities.native_permissions.get("crush");

        let mut allowed_tools = render_rules_to_strings(&perms.allow);
        if let Some(n) = native_perms {
            allowed_tools.extend(n.allow.iter().cloned());
        }

        // #1325 (security-audit on #1321): that "ask/deny needs no rendering"
        // reasoning only held while `allow` was inert too — before #1321's
        // tool-name mapping, an unscoped allow rendered a PascalCase string
        // that never matched a real Crush tool, so the deny-by-default
        // fallback covered every case by accident. Now that `allow` lands as
        // a real, matching grant, a tool also named in `deny`/`ask` must be
        // withheld here — Crush has no `denied_tools` of its own to enforce
        // it. Unmapped deny/ask tools are skipped, not hard-errored: there's
        // no Crush grant to withhold in the first place.
        let withheld: std::collections::BTreeSet<&str> = perms
            .deny
            .iter()
            .chain(&perms.ask)
            .filter_map(|rule| crush_tool_name(&rule.tool))
            .collect();
        allowed_tools.retain(|t| !withheld.contains(t.as_str()));

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
            super::overlay_native_json(
                &mut mcp_value,
                manifest.capabilities.native_mcp.get("crush"),
                "native_mcp.crush",
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
        let mut providers_value = render_model_providers(&manifest.capabilities.model_providers)?;
        super::overlay_native_json(
            &mut providers_value,
            manifest.capabilities.native_model_providers.get("crush"),
            "native_model_providers.crush",
        )?;
        if providers_value.as_object().is_some_and(|o| !o.is_empty()) {
            doc.insert("providers".into(), providers_value);
        }

        // Default models (fix 1 pattern): omit "models" key if none, after the
        // native_default_models.crush overlay (#1031) — mirrors the providers
        // block above, so a native-only per-role override (`reasoning_effort`,
        // `think`, `max_tokens`) can populate "models" even with no modeled
        // `default_models` entry for that role.
        let mut models_value = render_default_models(&manifest.capabilities.default_models);
        super::overlay_native_json(
            &mut models_value,
            manifest.capabilities.native_default_models.get("crush"),
            "native_default_models.crush",
        )?;
        if models_value.as_object().is_some_and(|o| !o.is_empty()) {
            doc.insert("models".into(), models_value);
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
        // Use native_permissions.crush / native_hooks.crush / native_mcp.crush /
        // native_model_providers.crush / native_default_models.crush instead.
        if let Some(native) = manifest.native.get("crush") {
            super::reject_modeled_native_keys(native, CRUSH_MODELED_KEYS, "crush")?;
        }
        let mut doc_value = serde_json::Value::Object(doc);
        super::overlay_native_json(&mut doc_value, manifest.native.get("crush"), "native.crush")?;
        // #1270: a native null on a key already rendered must delete the key
        // rather than persist an explicit JSON null (mirrors #1264's fix for
        // the Claude Code adapter's settings.json). Runs after the last
        // overlay so it catches every layer.
        super::strip_json_nulls(&mut doc_value);

        // 7. Write crush.json
        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(CRUSH_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;

        Ok(owned)
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        super::emit_hook_context(hook_event_name, text)
    }
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
/// Use the dedicated `native_permissions.crush` / `native_hooks.crush` / `native_mcp.crush` /
/// `native_model_providers.crush` / `native_default_models.crush` channels instead, which
/// merge in the safe direction.
const CRUSH_MODELED_KEYS: &[&str] = &["permissions", "hooks", "mcp", "lsp", "providers", "models"];

fn render_rules_to_strings(rules: &[crate::config::PermissionRule]) -> Vec<String> {
    rules.iter().flat_map(render_permission_rule).collect()
}

/// Map a neutral permission-rule tool name (Claude Code's vocabulary —
/// `Bash`, `Read`, `WebFetch`, ...) to Crush's own tool identifier.
///
/// #1321: source-verified against `charmbracelet/crush`'s `allToolNames()`
/// (`internal/config/config.go`) — Crush's names are lowercase and not
/// always a simple case change (`Read` -> `view`, `WebFetch` -> `fetch`, not
/// `webfetch`). `allToolNames()` also lists `agentic_fetch`/`web_search` as
/// separate, more specialized fetch/search tools with no direct Claude Code
/// equivalent — `WebFetch` maps to the base `fetch` tool, not those.
///
/// `Edit`/`MultiEdit` map to Crush's `edit`/`multiedit`, but those Crush
/// tools create missing files and parent directories on an empty
/// `old_string` (`internal/agent/tools/edit.go`'s `createNewFile`) — Claude
/// Code's `Edit` errors on a nonexistent path instead, requiring `Write` to
/// create one. Allowing `Edit` for Crush therefore also allows file
/// creation, which the neutral name alone doesn't imply; see the Crush
/// capability map in `engines.md` for the documented caveat.
///
/// Returns `None` for a neutral name with no Crush equivalent (`Task`,
/// `NotebookEdit`, ...). The neutral permission list is shared across every
/// engine, so a rule naming a Claude-Code-only tool is a normal, valid
/// config, not a user error specific to Crush — hard-erroring the entire
/// Crush materialize over one such rule would be a worse outcome than
/// [`render_permission_rule`]'s existing drop-and-log handling for
/// pattern/path scoping, so unmapped tools get the same treatment instead
/// of a harder failure mode.
fn crush_tool_name(neutral: &str) -> Option<&'static str> {
    Some(match neutral {
        "Bash" => "bash",
        "Read" => "view",
        "Write" => "write",
        "Edit" => "edit",
        "MultiEdit" => "multiedit",
        "Glob" => "glob",
        "Grep" => "grep",
        "LS" => "ls",
        "WebFetch" => "fetch",
        "TodoWrite" => "todos",
        _ => return None,
    })
}

/// Render a rule for Crush's `permissions.allowed_tools`.
///
/// #1306: source-verified against `charmbracelet/crush`'s
/// `internal/permission/permission.go` (`Request()`) — the allowlist check is
/// `slices.Contains(s.allowedTools, opts.ToolName)` or `slices.Contains(...,
/// opts.ToolName + ":" + opts.Action)`, both exact string equality against a
/// *fixed* per-tool-type action string (`"execute"` for bash, `"write"` for
/// edit/write, `"read"` for view, etc. — never the actual command or file
/// path). Crush has no concept of matching a command pattern or a file path
/// at all, so a `tool(pattern)` or `tool(path)` entry can never match either
/// comparison shape: it looks like a scoped allow rule once rendered, but is
/// silently inert.
///
/// A pattern/path-scoped rule is therefore dropped entirely rather than
/// widened to the bare tool name. Rendering the bare name would grant *every*
/// call to that tool — broader than what was asked for — for a security
/// control (this list is what skips Crush's interactive approval prompt).
/// Trading a narrow, unenforceable scope for a broad, enforced one is the
/// wrong direction for an allowlist: dropping the rule keeps Crush's
/// deny-by-default posture (the tool still prompts) instead of silently
/// over-granting. `tracing::error!`, not `warn!` — this codebase's default
/// `EnvFilter` (`src/main.rs`) drops `warn!` when `RUST_LOG` is unset, so a
/// downgrade on a permission-relevant path needs `error!` to actually surface
/// (see #1139, the same trap already fixed at six other call sites).
///
/// #1321: an unscoped rule's tool name is also translated to Crush's own
/// identifier via [`crush_tool_name`] — llmenv's neutral vocabulary is Claude
/// Code's PascalCase (`Bash`, `Read`, `WebFetch`), which never matched
/// Crush's lowercase names before this mapping existed. A neutral tool with
/// no Crush equivalent gets the same drop-and-log treatment as scoping —
/// see [`crush_tool_name`] for why this isn't a hard error.
fn render_permission_rule(rule: &crate::config::PermissionRule) -> Vec<String> {
    if let Some(pattern) = &rule.pattern {
        tracing::error!(
            "crush: allowed_tools has no pattern matching; rule `{}` + pattern `{pattern}` \
             cannot be expressed for Crush and is dropped rather than widened to allow \
             ALL `{}` calls — this tool will still prompt for approval",
            rule.tool,
            rule.tool
        );
        return Vec::new();
    }
    if !rule.paths.is_empty() {
        tracing::error!(
            "crush: allowed_tools has no path matching; rule `{}` + paths {:?} cannot be \
             expressed for Crush and is dropped rather than widened to allow ALL `{}` \
             calls — this tool will still prompt for approval",
            rule.tool,
            rule.paths,
            rule.tool
        );
        return Vec::new();
    }
    match crush_tool_name(&rule.tool) {
        Some(name) => vec![name.to_string()],
        None => {
            tracing::error!(
                "crush: no equivalent tool for neutral permission rule `{}` — Crush has no \
                 matching tool, so this rule is dropped and can never take effect. Remove it, \
                 or author a Crush-native rule directly via `native_permissions.crush`.",
                rule.tool
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::{
        CRUSH_JSON_FILE, CRUSH_MODELED_KEYS, CrushAdapter, SUPPORTED_HOOK_EVENTS, crush_tool_name,
        render_permission_rule,
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
                task_tracker: Some(crate::config::TaskTracker {
                    enabled: true,
                    ..Default::default()
                }),
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
            wakeup_max_tokens: None,
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

    /// #1325 (security-audit on #1321): once an unscoped allow rule became a
    /// real, matching Crush grant, an explicit deny/ask for the same tool
    /// must withhold it — Crush has no `denied_tools` of its own, so this
    /// neutral-side cross-check is the only thing standing in for one. Before
    /// #1321's tool-name mapping, `allow` never matched anything for real
    /// Crush, so this exact conflict was harmless by accident; #1321 made it
    /// live.
    #[test]
    fn materialize_deny_withholds_a_conflicting_allow() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.allow.push(PermissionRule {
            tool: "Bash".into(),
            pattern: None,
            paths: vec![],
        });
        caps.permissions.deny.push(PermissionRule {
            tool: "Bash".into(),
            pattern: Some("rm -rf *".into()),
            paths: vec![],
        });
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("permissions").is_none(),
            "a denied tool must never appear in allowed_tools, even if also allowed: {doc}"
        );
    }

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
        // #1321: the neutral name is translated to Crush's own tool identifier.
        assert!(allowed.contains(&serde_json::json!("bash")));
        assert!(!allowed.contains(&serde_json::json!("Bash")));
    }

    /// #1321: source-verified against `charmbracelet/crush`'s
    /// `allToolNames()` (`internal/config/config.go`) — Crush's tool names
    /// are lowercase and not always a simple case change of the neutral
    /// (Claude Code) name.
    #[test]
    fn materialize_maps_every_documented_neutral_tool_to_its_crush_name() {
        for (neutral, crush) in [
            ("Bash", "bash"),
            ("Read", "view"),
            ("Write", "write"),
            ("Edit", "edit"),
            ("MultiEdit", "multiedit"),
            ("Glob", "glob"),
            ("Grep", "grep"),
            ("LS", "ls"),
            ("WebFetch", "fetch"),
            ("TodoWrite", "todos"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut caps = Capabilities::default();
            caps.permissions.allow.push(PermissionRule {
                tool: neutral.into(),
                pattern: None,
                paths: vec![],
            });
            CrushAdapter
                .materialize(&manifest_with_caps(caps), tmp.path())
                .map_err(|e| format!("{neutral} should map to a Crush tool: {e}"))
                .unwrap();
            let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let allowed = doc["permissions"]["allowed_tools"].as_array().unwrap();
            assert!(
                allowed.contains(&serde_json::json!(crush)),
                "{neutral} should render as {crush}, got {allowed:?}"
            );
        }
    }

    /// #1325 (security-audit on #1321): a neutral tool with no Crush
    /// equivalent (e.g. `Task`) is dropped, not hard-errored — the neutral
    /// permission list is shared across every engine, so a rule naming a
    /// Claude-Code-only tool is a normal, valid config that must not break
    /// Crush materialize wholesale. Same drop-and-log treatment #1306 uses
    /// for pattern/path scoping.
    #[test]
    fn materialize_unmapped_neutral_tool_is_dropped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = Capabilities::default();
        caps.permissions.allow.push(PermissionRule {
            tool: "Task".into(),
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
            "an unmapped tool must be dropped, not fail materialize or render a bogus \
             entry: {doc}"
        );
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

    /// #1306: Crush's `allowed_tools` matches only the bare tool name or
    /// `tool:action` against a fixed per-tool-type action string — never a
    /// command pattern. A `Bash(ls*)`-shaped entry can never match, so it is
    /// dropped rather than widened to a bare-tool grant: substituting a
    /// broader allow for a narrower one is the wrong direction for a
    /// security control, and dropping the rule keeps Crush's deny-by-default
    /// posture (the tool still prompts) instead of silently over-granting.
    #[test]
    fn materialize_permission_with_pattern_is_dropped_not_widened() {
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
        assert!(
            doc.get("permissions").is_none(),
            "a pattern-scoped rule that can't be expressed for Crush must produce no \
             permissions output, not a widened bare-tool grant: {doc}"
        );
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

    /// #1270: `native.crush: {options: null}` must delete a key the render
    /// already emitted, mirroring #1264's fix for the Claude Code adapter's
    /// `settings.json`. `options` is rendered whenever the llmenv skill is
    /// materialized (a feature flag, not an external fixture), and it is not
    /// in `CRUSH_MODELED_KEYS`, so the catch-all accepts overriding it.
    #[test]
    fn materialize_native_null_removes_a_rendered_crush_key() {
        let tmp = tempfile::tempdir().unwrap();
        let caps = Capabilities {
            features: Some(crate::config::Features {
                task_tracker: Some(crate::config::TaskTracker {
                    enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut manifest = manifest_with_caps(caps);
        let frag: serde_yaml::Value = serde_yaml::from_str("options: null").unwrap();
        manifest.native.insert("crush".into(), frag);
        CrushAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("options").is_none(),
            "`native.crush.options: null` must delete the key, got: {doc}"
        );
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

    // ── overlay_native_json (shared) ──────────────────────────────────────────

    #[test]
    fn overlay_native_crush_none_is_noop() {
        let mut dst = serde_json::json!({ "k": 1 });
        let before = dst.clone();
        super::super::overlay_native_json(&mut dst, None, "native.crush").unwrap();
        assert_eq!(dst, before);
    }

    #[test]
    fn overlay_native_crush_merges_keys() {
        let mut dst = serde_json::json!({ "a": 1 });
        let frag: serde_yaml::Value = serde_yaml::from_str("b: 2").unwrap();
        super::super::overlay_native_json(&mut dst, Some(&frag), "native.crush").unwrap();
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
        assert_eq!(render_permission_rule(&rule), vec!["bash"]);
    }

    #[test]
    fn render_tool_with_pattern_is_dropped() {
        let rule = PermissionRule {
            tool: "Bash".into(),
            pattern: Some("ls*".into()),
            paths: vec![],
        };
        assert_eq!(render_permission_rule(&rule), Vec::<String>::new());
    }

    #[test]
    fn render_tool_with_paths_is_dropped() {
        let rule = PermissionRule {
            tool: "Read".into(),
            pattern: None,
            paths: vec!["src/".into(), "tests/".into()],
        };
        assert_eq!(render_permission_rule(&rule), Vec::<String>::new());
    }

    #[test]
    fn render_unmapped_tool_is_dropped() {
        let rule = PermissionRule {
            tool: "Task".into(),
            pattern: None,
            paths: vec![],
        };
        assert_eq!(render_permission_rule(&rule), Vec::<String>::new());
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
            wakeup_max_tokens: None,
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
            wakeup_max_tokens: None,
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

        // permissions: #1306 — the only allow rule is pattern-scoped, which Crush
        // can't express, so it's dropped rather than widened to a bare-tool
        // grant; the ask rule produces no output either — net result, no
        // permissions key at all.
        assert!(doc.get("permissions").is_none());
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

    // ── materialize: native_model_providers.crush merged into providers (#1008) ──

    fn crush_doc_with_native_providers(mut caps: Capabilities, yaml: &str) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        caps.native_model_providers.insert(
            "crush".into(),
            serde_yaml::from_str(yaml).expect("test fragment must be valid YAML"),
        );
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn caps_with_mtplx_provider() -> Capabilities {
        Capabilities {
            model_providers: vec![llmenv_config::ModelProvider {
                id: "mtplx".into(),
                name: Some("modeled".into()),
                base_url: Some("http://localhost:8080/v1".into()),
                ..Default::default()
            }],
            ..Capabilities::default()
        }
    }

    #[test]
    fn materialize_native_model_providers_without_modeled_providers() {
        // No `model_providers` at all: the fragment alone must still render a
        // `providers` block — otherwise the escape hatch is unusable on its own.
        let doc = crush_doc_with_native_providers(
            Capabilities::default(),
            "mtplx:\n  id: mtplx\n  api_endpoint: http://localhost:8080/v1\n",
        );
        assert_eq!(
            doc["providers"]["mtplx"]["api_endpoint"],
            serde_json::json!("http://localhost:8080/v1")
        );
    }

    #[test]
    fn materialize_native_model_providers_deep_merges_onto_rendered_providers() {
        let doc = crush_doc_with_native_providers(
            caps_with_mtplx_provider(),
            "mtplx:\n  extra_headers:\n    X-Tenant: acme\n",
        );
        assert_eq!(
            doc["providers"]["mtplx"]["extra_headers"]["X-Tenant"],
            serde_json::json!("acme"),
            "unmodeled provider key must be injected"
        );
        assert_eq!(
            doc["providers"]["mtplx"]["api_endpoint"],
            serde_json::json!("http://localhost:8080/v1"),
            "deep merge must preserve sibling keys rendered from model_providers"
        );
    }

    #[test]
    fn materialize_native_model_providers_overrides_on_collision() {
        let doc =
            crush_doc_with_native_providers(caps_with_mtplx_provider(), "mtplx:\n  name: native\n");
        assert_eq!(
            doc["providers"]["mtplx"]["name"],
            serde_json::json!("native"),
            "the native fragment is the higher-precedence layer on collision"
        );
    }

    #[test]
    fn materialize_native_model_providers_empty_mapping_omits_providers_key() {
        let doc = crush_doc_with_native_providers(Capabilities::default(), "{}");
        assert!(
            doc.get("providers").is_none(),
            "an empty fragment must not emit an empty \"providers\" object"
        );
    }

    /// `models` is a JSON *array* in crush's provider schema, so a fragment's
    /// `models` entries append rather than patch (see the caveat in the docs).
    #[test]
    fn materialize_native_model_providers_crush_models_append_not_patch() {
        let mut caps = caps_with_mtplx_provider();
        caps.model_providers[0].models = vec![llmenv_config::ModelSource {
            id: "gpt-oss".into(),
            ..Default::default()
        }];
        let doc = crush_doc_with_native_providers(
            caps,
            "mtplx:\n  models:\n    - id: gpt-oss\n      can_reason: true\n",
        );
        let models = doc["providers"]["mtplx"]["models"].as_array().unwrap();
        assert_eq!(
            models.len(),
            2,
            "crush's models is a list — merge_json concatenates instead of \
             patching by id; docs must not promise per-model override here"
        );
    }

    // ── materialize: native_default_models.crush merged into models (#1031) ──

    fn crush_doc_with_native_default_models(
        mut caps: Capabilities,
        yaml: &str,
    ) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        caps.native_default_models.insert(
            "crush".into(),
            serde_yaml::from_str(yaml).expect("test fragment must be valid YAML"),
        );
        CrushAdapter
            .materialize(&manifest_with_caps(caps), tmp.path())
            .unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn caps_with_large_role() -> Capabilities {
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "large".to_string(),
                llmenv_config::ModelRef {
                    provider: "mtplx".into(),
                    model: "qwen3".into(),
                },
            )]),
            ..Capabilities::default()
        }
    }

    #[test]
    fn materialize_native_default_models_without_modeled_default_models() {
        // No `default_models` at all: the fragment alone must still render a
        // `models` block — otherwise the escape hatch is unusable on its own.
        let doc = crush_doc_with_native_default_models(
            Capabilities::default(),
            "large:\n  provider: mtplx\n  model: qwen3\n",
        );
        assert_eq!(doc["models"]["large"]["model"], serde_json::json!("qwen3"));
    }

    #[test]
    fn materialize_native_default_models_deep_merges_onto_rendered_models() {
        let doc = crush_doc_with_native_default_models(
            caps_with_large_role(),
            "large:\n  reasoning_effort: high\n",
        );
        assert_eq!(
            doc["models"]["large"]["reasoning_effort"],
            serde_json::json!("high"),
            "unmodeled per-role key must be injected"
        );
        assert_eq!(
            doc["models"]["large"]["model"],
            serde_json::json!("qwen3"),
            "deep merge must preserve sibling keys rendered from default_models"
        );
    }

    #[test]
    fn materialize_native_default_models_overrides_on_collision() {
        let doc = crush_doc_with_native_default_models(
            caps_with_large_role(),
            "large:\n  model: native-override\n",
        );
        assert_eq!(
            doc["models"]["large"]["model"],
            serde_json::json!("native-override"),
            "the native fragment is the higher-precedence layer on collision"
        );
    }

    #[test]
    fn materialize_native_default_models_empty_mapping_omits_models_key() {
        let doc = crush_doc_with_native_default_models(Capabilities::default(), "{}");
        assert!(
            doc.get("models").is_none(),
            "an empty fragment must not emit an empty \"models\" object"
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
            let err = super::super::reject_modeled_native_keys(&frag, CRUSH_MODELED_KEYS, "crush")
                .unwrap_err();
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
            // Store-only events (SessionStart/SessionEnd) return empty;
            // all other events return valid JSON wrapping the text.
            prop_assert!(
                out.is_empty() || serde_json::from_str::<serde_json::Value>(&out).is_ok(),
                "output must be empty or valid JSON; event={event}, text={text}, got={out}"
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
        fn prop_render_permission_rule_scoped_is_dropped_unscoped_is_mapped(
            tool in prop::sample::select(&["Bash", "Read", "Write", "Edit", "Glob", "Grep"][..]),
            pattern in prop::option::of("[a-z*]{1,15}"),
            paths in prop::collection::vec("[a-z/]{1,15}", 0..5),
        ) {
            let scoped = pattern.is_some() || !paths.is_empty();
            let rule = crate::config::PermissionRule {
                tool: tool.to_string(),
                pattern,
                paths,
            };
            // #1306: Crush's allowed_tools has no pattern/path matching. A
            // scoped rule can never be expressed, so it's dropped (fail-closed
            // — the tool still prompts) rather than widened to a bare-tool
            // grant. #1321: an unscoped rule renders the mapped Crush tool
            // name, not the neutral one verbatim. `tool` is restricted to
            // names known to map successfully — the mapping's own
            // success/failure per name is covered by
            // `materialize_maps_every_documented_neutral_tool_to_its_crush_name`
            // and `render_unmapped_tool_is_dropped`.
            let expected = if scoped {
                Vec::new()
            } else {
                vec![crush_tool_name(tool).unwrap().to_string()]
            };
            prop_assert_eq!(render_permission_rule(&rule), expected);
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

        // #1325 (property-test-gap-finder on #1321's deny/ask cross-check):
        // arbitrary allow/deny/ask combinations must never let a denied or
        // asked tool survive into allowed_tools — mirrors
        // generate_settings_json_permission_buckets_never_overlap's coverage
        // of the equivalent claude_code.rs invariant.
        #[test]
        fn prop_crush_allowed_tools_never_contains_a_denied_or_asked_tool(
            allow_tools in prop::collection::vec(
                prop::sample::select(&["Bash", "Read", "Write", "Edit", "Glob", "Grep"][..]),
                0..4,
            ),
            deny_tools in prop::collection::vec(
                prop::sample::select(&["Bash", "Read", "Write", "Edit", "Glob", "Grep"][..]),
                0..4,
            ),
            ask_tools in prop::collection::vec(
                prop::sample::select(&["Bash", "Read", "Write", "Edit", "Glob", "Grep"][..]),
                0..4,
            ),
        ) {
            let bare = |tool: &str| PermissionRule {
                tool: tool.to_string(),
                pattern: None,
                paths: Vec::new(),
            };
            let mut caps = Capabilities::default();
            caps.permissions.allow = allow_tools.iter().map(|t| bare(t)).collect();
            caps.permissions.deny = deny_tools.iter().map(|t| bare(t)).collect();
            caps.permissions.ask = ask_tools.iter().map(|t| bare(t)).collect();

            let tmp = tempfile::tempdir().unwrap();
            CrushAdapter
                .materialize(&manifest_with_caps(caps), tmp.path())
                .unwrap();
            let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let allowed: std::collections::BTreeSet<&str> = doc["permissions"]["allowed_tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect();
            let withheld: std::collections::BTreeSet<&str> = deny_tools
                .iter()
                .chain(&ask_tools)
                .filter_map(|t| crush_tool_name(t))
                .collect();
            prop_assert!(
                allowed.is_disjoint(&withheld),
                "allowed_tools must never contain a denied/asked tool: allowed={allowed:?} \
                 withheld={withheld:?}"
            );
        }

        // ── P2: overlay_native_json (shared) ──────────────────────────────────

        #[test]
        fn prop_overlay_native_crush_idempotent(
            fragment in prop::collection::hash_map("[a-z]{1,8}", 0i64..1000, 0..5),
        ) {
            let frag_yaml: serde_yaml::Value = serde_yaml::to_value(&fragment).unwrap();

            let mut once = serde_json::json!({});
            super::super::overlay_native_json(&mut once, Some(&frag_yaml), "native.crush").unwrap();

            let mut twice = serde_json::json!({});
            super::super::overlay_native_json(&mut twice, Some(&frag_yaml), "native.crush").unwrap();
            super::super::overlay_native_json(&mut twice, Some(&frag_yaml), "native.crush").unwrap();

            prop_assert_eq!(once, twice, "applying the same fragment twice must equal applying it once");
        }

        #[test]
        fn prop_overlay_native_crush_no_panic(
            fragment in arb_yaml_value(3),
        ) {
            let mut dst = serde_json::json!({"existing": "value"});
            let _ = super::super::overlay_native_json(&mut dst, Some(&fragment), "native.crush");
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
