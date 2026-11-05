use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use schemars::JsonSchema;

use super::{AgentAdapter, binary_on_path};
use crate::mcp::resolve::ResolvedKind;
use crate::merge::MergedManifest;

/// Adapter for opencode: writes `AGENTS.md` and `opencode.json` into the
/// cache dir and exports `OPENCODE_CONFIG_DIR` so opencode discovers them.
///
/// Skills use the claude-compatible `SKILL.md` format opencode reads natively.
/// Hooks are bridged by the `plugin/llmenv.js` plugin and its `plugin/llmenv-hooks.json` table.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpencodeAdapter;

mod hook_bridge;
mod models;
mod permissions;
mod plugin_parse;
mod tiers;
mod translate;
use hook_bridge::SUPPORTED_HOOK_EVENTS;
use models::{render_opencode_default_models, render_opencode_providers};
#[cfg(test)]
use translate::{OPENCODE_AGENT_KEYS, dropped_agent_keys, split_frontmatter};
use translate::{translate_agent_md, translate_command_md};

const OPENCODE_JSON_FILE: &str = "opencode.json";
const OPENCODE_SCHEMA_FILE: &str = "opencode.schema.json";
/// The background-server settings that opencode reads from its config folder.
const OPENCODE_SERVICE_FILE: &str = "service.json";

// ── Typed output structs for opencode.json ──

/// A single `mcp.<name>` entry in `opencode.json`. This is the type the
/// MCP-assembly step in `materialize()` actually constructs and serializes
/// — both the JSON output and the generated schema (via
/// `#[schemars(with = ...)]` on `OpencodeConfig::mcp`) derive from it, so
/// there is exactly one definition of what an MCP entry looks like.
#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
#[serde(tag = "type", rename_all = "lowercase")]
enum McpEntry {
    Local {
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        environment: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
    },
    Remote {
        /// Deliberately unvalidated passthrough — see the doc comment on
        /// `ResolvedKind::Remote::url` (#1017) for why.
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
    },
}

/// Top-level structure for the opencode.json config document.
/// Constructed from the merged manifest, serialized to Value, then native
/// overlay keys are deep-merged at the Value level.
#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeConfig {
    /// Native opencode JS plugins (e.g. context-mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<Vec<String>>,
    /// MCP server configs — kept as Value because entries go through
    /// per-server native_mcp overlay after construction; `#[schemars(with)]`
    /// tells schemars to describe the field as if it were typed, without
    /// changing what's actually serialized.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<BTreeMap<String, McpEntry>>")]
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
    /// Permission rules, in the order opencode evaluates them.
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<Vec<permissions::Rule>>,
    /// Custom/self-hosted model provider endpoints, keyed by provider id. Kept
    /// as Value because the block goes through a `native_model_providers`
    /// overlay after construction (same reason as `mcp`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<BTreeMap<String, OpencodeProviderEntry>>")]
    provider: Option<serde_json::Value>,
    /// Default model, as `provider_id/model_id` — from `default_models.large`.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Lightweight-task model, as `provider_id/model_id` — from
    /// `default_models.small`. opencode has no slot for any other role name.
    #[serde(skip_serializing_if = "Option::is_none")]
    small_model: Option<String>,
}

/// A `provider.<id>` entry in `opencode.json`, matching opencode's
/// `ProviderConfig` schema (`packages/core/src/v1/config/provider.ts`).
#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeProviderEntry {
    /// AI SDK package, e.g. `@ai-sdk/openai-compatible`, `@ai-sdk/anthropic`.
    npm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    options: OpencodeProviderOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<BTreeMap<String, OpencodeModelEntry>>,
}

/// `provider.<id>.options` — the named keys opencode's `StructWithRest`
/// schema recognizes; opencode itself tolerates arbitrary extra keys here,
/// but this struct only renders what llmenv actually sets.
#[derive(serde::Serialize, Default, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeProviderOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "baseURL")]
    base_url: Option<String>,
    #[serde(rename = "apiKey", skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<BTreeMap<String, String>>,
}

/// A `provider.<id>.models.<model_id>` entry, matching opencode's `Model`
/// config schema. Only the subset llmenv's `ModelSource` can populate.
#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeModelEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<OpencodeModelLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<OpencodeModelCost>,
    /// Only the input side — llmenv's `ModelSource.modalities` models input
    /// modalities only.
    #[serde(skip_serializing_if = "Option::is_none")]
    modalities: Option<OpencodeModalities>,
}

#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeModelLimit {
    context: u32,
    output: u32,
}

#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeModelCost {
    input: f64,
    output: f64,
    #[serde(rename = "cache_read", skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(rename = "cache_write", skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
}

#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
struct OpencodeModalities {
    input: Vec<String>,
}

/// An LSP server entry in opencode.json.
#[derive(serde::Serialize, JsonSchema)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq, Debug))]
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

impl AgentAdapter for OpencodeAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    /// Active when `OPENCODE_CONFIG_DIR` is set or `opencode` is on `PATH`.
    fn is_active(&self) -> bool {
        if std::env::var_os("OPENCODE_CONFIG_DIR").is_some() {
            return true;
        }
        binary_on_path("opencode")
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

    fn supports_output_styles(&self) -> bool {
        false
    }

    /// Every map this adapter reads. `native_hooks` and `native_plugins` are
    /// absent even though `supports_plugins()` is true and
    /// `supported_hook_events()` is non-empty: hooks render from the neutral
    /// `capabilities.hooks` through the JS shim, and plugins render from the
    /// resolved plugin list — neither path consults a per-engine fragment.
    fn native_maps(&self) -> &'static [&'static str] {
        use crate::adapter::native_keys as nk;
        &[
            nk::NATIVE_PERMISSIONS,
            nk::NATIVE_MCP,
            nk::NATIVE_MODEL_PROVIDERS,
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
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        let (_, temp_vars) = crate::adapter::temp_dir::temp_dir_vars(state_dir)?;
        // opencode 2.x also loads the skills in `~/.claude` and a project `.claude` folder on
        // every session, outside llmenv's scope rules. This variable turns those roots off (#2843).
        Ok(vec![
            ("OPENCODE_CONFIG_DIR".into(), dir.to_owned()),
            ("OPENCODE_DISABLE_CLAUDE_CODE".into(), "1".into()),
        ]
        .into_iter()
        .chain(temp_vars)
        .collect())
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        super::skills::create_dir_owner_only(out)?;

        let mut owned: Vec<PathBuf> = Vec::new();

        // 1. AGENTS.md
        // #1269: skip the file entirely when nothing resolved, rather than
        // leaving a 0-byte AGENTS.md. Staying out of `owned` also means a copy
        // written by an earlier render is reconciled away as a ghost.
        super::skills::reject_hardcoded_config_path(&manifest.agents_md, "AGENTS.md")?;
        if !manifest.agents_md.trim().is_empty() {
            crate::paths::write_owner_only(&out.join("AGENTS.md"), manifest.agents_md.as_bytes())?;
            owned.push(PathBuf::from("AGENTS.md"));
        }

        // 2. rules/*.md — written verbatim; paths collected for instructions[].
        // opencode 2.x looks for a relative entry upward from the session folder, not in the
        // config folder, so each entry is an absolute path (#2842).
        let out_abs = std::path::absolute(out)?;
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
            instructions.push(out_abs.join(&r.rel).to_string_lossy().into_owned());
            owned.push(r.rel.clone());
        }

        // 3. First-class skills (declared via `capabilities.skills`).
        let skill_owned =
            crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;
        owned.extend(skill_owned);

        // Built-in `llmenv` skill: one reference file per enabled first-party
        // feature. No-op when none are enabled.
        owned.extend(crate::adapter::llmenv_skill::materialize_llmenv_skill(
            out, manifest,
        )?);

        // #1130/#1333/#1335: opencode has no native output-style concept —
        // render each declared output style as a generated skill instead.
        // Resolve every plugin's payload dir and discover its skills *once*
        // here (before writing anything), then reuse this same discovery
        // both for the collision check below and for the actual skill
        // write in the plugin loop (step 4d) — a second, separate directory
        // walk there could observe a different result (TOCTOU).
        // The plugin reference travels with its payload/skills in the same
        // tuple (rather than a `Vec` zipped positionally against
        // `manifest.plugins` afterwards) so the pairing can't silently drift
        // if this construction ever grows a `filter`/skip step later
        // (security-audit, #1335) — Crush already has exactly such a skip.
        let plugin_payloads: Vec<(
            &crate::plugins::resolve::ResolvedPlugin,
            PathBuf,
            Vec<crate::config::SkillSource>,
        )> = manifest
            .plugins
            .iter()
            .map(|plugin| {
                let payload = super::resolve_plugin_payload(plugin, &manifest.marketplaces)?;
                let skills = crate::adapter::skills::discover_plugin_skills(&payload)?;
                anyhow::Ok((plugin, payload, skills))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let projected_plugin_skills: Vec<crate::adapter::output_styles::ProjectedPluginSkill> =
            plugin_payloads
                .iter()
                .flat_map(|(plugin, _, skills)| {
                    skills.iter().map(move |s| {
                        crate::adapter::output_styles::ProjectedPluginSkill {
                            skill_name: s.name.clone(),
                            plugin_name: plugin.plugin.clone(),
                        }
                    })
                })
                .collect();
        crate::adapter::output_styles::reject_plugin_skill_collisions(
            &manifest.capabilities.output_styles,
            &projected_plugin_skills,
        )?;
        for style in &manifest.capabilities.output_styles {
            owned.extend(crate::adapter::output_styles::write_output_style_as_skill(
                out, style,
            )?);
        }

        // 4. Plugin content translation (commands, agents, MCP, skills, hooks).
        let mut plugin_mcp_entries: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        let mut plugin_skill_paths: Vec<PathBuf> = Vec::new();
        let mut plugin_hooks: Vec<crate::config::Hook> = Vec::new();
        // Hoisted: command/ dir needed by plugin and bundle sections below.
        std::fs::create_dir_all(out.join("command"))?;

        for (plugin, payload, plugin_skills) in &plugin_payloads {
            // context-mode's native opencode plugin exports the 1.x shape, which opencode 2.0
            // refuses to load (#2840), so it runs like any other plugin: its MCP server and its
            // hooks go through the llmenv bridge.
            if plugin.plugin == crate::config::CONTEXT_MODE_PLUGIN {
                eprintln!(
                    "warning: context-mode has no opencode 2.x plugin yet, so opencode gets its \
                     MCP server and hooks only. Fix: none needed; this changes when context-mode \
                     ships a 2.x build"
                );
            }
            // 4a. Plugin-provided MCP (LLM_PROVIDER_MCP_JSON).
            {
                let mcp_json_path = payload.join("LLM_PROVIDER_MCP_JSON");
                // #915: a missing file yields Ok(empty); only a real read/parse
                // error warns, instead of an exists() guard masking permission
                // errors as "no MCP config".
                match parse_plugin_mcp_entries(&mcp_json_path, &plugin.plugin) {
                    Ok(entries) => plugin_mcp_entries.extend(entries),
                    Err(e) => eprintln!(
                        "warning: {e} — skipping this plugin's MCP servers, other plugins \
                         are unaffected."
                    ),
                }
            }

            // 4b. Translate commands/ from plugin
            let cmd_dir = payload.join("commands");
            if let Some(entries) = crate::paths::read_dir_optional(&cmd_dir)? {
                for entry in entries {
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
                    let source = crate::paths::bounded::asset_text(&path)?;
                    let translated = translate_command_md(&source)?;
                    let out_name = format!("command/__plugin_{name}.md");
                    crate::paths::write_owner_only(&out.join(&out_name), translated.as_bytes())?;
                    plugin_skill_paths.push(PathBuf::from(&out_name));
                }
            }

            // 4c. Translate agents/ from plugin
            let agent_dir = payload.join("agents");
            if let Some(entries) = crate::paths::read_dir_optional(&agent_dir)? {
                for entry in entries {
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
                    let source = crate::paths::bounded::asset_text(&path)?;
                    let translated = translate_agent_md(&source)?;
                    // opencode 2.x reads agents from `agents/`; a file in `command/` is a slash
                    // command (#2844).
                    let out_name = format!("agents/{name}.md");
                    std::fs::create_dir_all(out.join("agents"))?;
                    crate::paths::write_owner_only(&out.join(&out_name), translated.as_bytes())?;
                    plugin_skill_paths.push(PathBuf::from(&out_name));
                }
            }

            // 4d. Plugin-projected skills (skills dir inside a plugin payload).
            // Reuses the `plugin_skills` discovered above (same walk the
            // collision check used) instead of a second directory read.
            let paths = crate::adapter::skills::write_first_class_skills(out, plugin_skills)?;
            plugin_skill_paths.extend(paths);

            // 4e. Plugin hooks from hooks/ dir — translate to top-level hooks.
            {
                let hooks_dir = payload.join("hooks");
                for manifest_name in ["hooks.json", "claude-codex-hooks.json"] {
                    let hooks_path = hooks_dir.join(manifest_name);
                    // #915: parse_plugin_hooks returns Ok(empty) for a missing
                    // manifest; a real read/parse error still warns + breaks.
                    let mut parsed = match parse_plugin_hooks(&hooks_path, payload, &plugin.plugin)
                    {
                        Ok(Some(parsed)) => parsed,
                        // Manifest of this name is absent — try the next candidate.
                        Ok(None) => continue,
                        Err(e) => {
                            eprintln!(
                                "warning: failed to parse '{}' for plugin '{}': {e} — \
                                     skipping this plugin's hooks, other plugins are unaffected.",
                                hooks_path.display(),
                                plugin.plugin,
                            );
                            break;
                        }
                    };
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
                        // Unreachable in practice: parse_plugin_hooks only ever
                        // constructs HookHandlerKind::Command hooks (non-"command"
                        // kinds are filtered out earlier, with their own warning).
                        // Kept defensively in case that invariant ever changes.
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
        // 7. MCP servers
        let config_mcp = {
            if !manifest.mcps.is_empty()
                || manifest.capabilities.native_mcp.contains_key("opencode")
            {
                let mut mcp_obj = serde_json::Map::new();
                for mcp in &manifest.mcps {
                    let entry = match &mcp.kind {
                        ResolvedKind::Stdio { command, args, env } => {
                            let cmd: Vec<String> = std::iter::once(command.clone())
                                .chain(args.iter().cloned())
                                .collect();
                            McpEntry::Local {
                                command: cmd,
                                environment: env.clone(),
                                headers: mcp.headers.clone(),
                                timeout: mcp.timeout,
                            }
                        }
                        ResolvedKind::Remote { url, transport: _ } => McpEntry::Remote {
                            url: url.clone(),
                            headers: mcp.headers.clone(),
                            timeout: mcp.timeout,
                        },
                    };
                    let value = serde_json::to_value(&entry).map_err(|err| {
                        anyhow::anyhow!(
                            "MCP server '{}': failed to serialize entry: {err}",
                            mcp.name
                        )
                    })?;
                    mcp_obj.insert(mcp.name.clone(), value);
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

        // 9. Permissions — an ordered rule list; opencode applies the last rule that matches.
        // Tier rules come first: opencode applies the last matching rule, so a user rule wins.
        let config_permissions = {
            let mut rules = tiers::tier_rules(manifest);
            if let Some(user) = permissions::render(
                &manifest.capabilities.permissions,
                manifest.capabilities.native_permissions.get("opencode"),
            )? {
                rules.extend(user);
            }
            (!rules.is_empty()).then_some(rules)
        };

        // 9b. Model providers + default models.
        let config_provider = {
            let rendered = render_opencode_providers(&manifest.capabilities.model_providers);
            let mut value = serde_json::to_value(&rendered)?;
            super::overlay_native_json(
                &mut value,
                manifest.capabilities.native_model_providers.get("opencode"),
                "native_model_providers.opencode",
            )?;
            value
                .as_object()
                .is_some_and(|o| !o.is_empty())
                .then_some(value)
        };
        let (config_model, config_small_model) =
            render_opencode_default_models(&manifest.capabilities);

        // 10. Native overlay — reject modeled keys, then deep-merge
        const OPENCODE_MODELED_KEYS: &[&str] = &[
            "instructions",
            "mcp",
            "lsp",
            "permission",
            "permissions",
            "provider",
            "model",
            "small_model",
        ];
        if let Some(native) = manifest.native.get("opencode") {
            super::reject_modeled_native_keys(native, OPENCODE_MODELED_KEYS, "opencode")?;
        }
        let config = OpencodeConfig {
            plugin: None,
            mcp: config_mcp,
            lsp: config_lsp,
            schema: format!("./{OPENCODE_SCHEMA_FILE}"),
            instructions: config_instructions,
            permissions: config_permissions,
            provider: config_provider,
            model: config_model,
            small_model: config_small_model,
        };
        let mut doc_value = serde_json::to_value(&config)?;
        super::overlay_native_json(
            &mut doc_value,
            manifest.native.get("opencode"),
            "native.opencode",
        )?;
        // #1270: a native null on a key already rendered must delete the key
        // rather than persist an explicit JSON null (mirrors #1264's fix for
        // the Claude Code adapter's settings.json). Runs after the last
        // overlay so it catches every layer.
        super::strip_json_nulls(&mut doc_value);
        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(OPENCODE_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;
        owned.push(PathBuf::from(OPENCODE_JSON_FILE));

        // opencode 2.x clients attach to one shared background server, which can run with the
        // config folder of another scope. `disabled` makes each client start a private server
        // that has this environment, as `--standalone` does (#2845).
        crate::paths::write_owner_only(&out.join(OPENCODE_SERVICE_FILE), br#"{"disabled":true}"#)?;
        owned.push(PathBuf::from(OPENCODE_SERVICE_FILE));

        // Write JSON Schema sidecar.
        if let Some(schema) = self.config_schema() {
            let schema_bytes = serde_json::to_vec_pretty(&schema)?;
            let schema_path = out.join(OPENCODE_SCHEMA_FILE);
            crate::paths::write_owner_only(&schema_path, &schema_bytes)?;
            owned.push(PathBuf::from(OPENCODE_SCHEMA_FILE));
        }

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

        let mcp_servers: Vec<&str> = manifest.mcps.iter().map(|m| m.name.as_str()).collect();
        owned.extend(hook_bridge::write(
            out,
            &all_hooks,
            &hook_bridge::pre_tool_hook_tools(manifest),
            &mcp_servers,
        )?);

        // 12. Bundle-level commands/agents from manifest.files
        for (rel, abs) in &manifest.files {
            if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in bundle file: {}", rel.display());
            }
            let rel_str = rel.to_string_lossy();
            if rel_str.starts_with("commands/") && rel_str.ends_with(".md") {
                let source = crate::paths::bounded::asset_text(abs).map_err(|e| {
                    anyhow::anyhow!("failed to read bundle file '{}': {e:#}", rel.display())
                })?;
                // The bundle copy at `rel` is already in a folder opencode reads, so the
                // translation replaces it and the command is not listed two times (#2844).
                let translated = translate_command_md(&source)?;
                write_translated(out, rel, &translated)?;
                owned.push(rel.clone());
            }
            if rel_str.starts_with("agents/") && rel_str.ends_with(".md") {
                let source = crate::paths::bounded::asset_text(abs).map_err(|e| {
                    anyhow::anyhow!("failed to read bundle file '{}': {e:#}", rel.display())
                })?;
                let translated = translate_agent_md(&source)?;
                if owned.contains(rel) {
                    eprintln!(
                        "warning: bundle agent '{}' replaces the plugin agent of the same name \
                         for opencode. Fix: rename one of the two agents",
                        rel.display()
                    );
                }
                write_translated(out, rel, &translated)?;
                owned.push(rel.clone());
            }
        }

        Ok(owned)
    }

    fn config_schema(&self) -> Option<serde_json::Value> {
        let value = schemars::schema_for!(OpencodeConfig).to_value();
        Some(crate::materialize::schema_gen::with_root_additional_properties(value))
    }

    fn emit_hook_context(&self, hook_event_name: &str, memory: &str, notes: &str) -> String {
        // The plugin adds SessionStart context to every model request of the session, as a
        // system part (`session.hook("context")` in `opencode/plugin.js`).
        super::emit_hook_context(
            hook_event_name,
            memory,
            notes,
            super::SessionStartContext::Accepted,
        )
    }
}

/// Write the translation of the bundle file `rel` over its copy in `out`.
fn write_translated(out: &Path, rel: &Path, translated: &str) -> anyhow::Result<()> {
    let dest = out.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::paths::write_owner_only(&dest, translated.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.display()))
}

use plugin_parse::{parse_plugin_hooks, parse_plugin_mcp_entries};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::permissions::{
        check_rule_ordering, glob_matches, opencode_tool_name, pattern_subsumes, patterns_overlap,
        wildcard_matches,
    };
    use super::*;
    use crate::adapter::hook_program::hook_command;
    use crate::adapter::yaml_value_kind_name;

    /// The `permissions` list of `doc`, folded to a `{action: effect | {resource: effect}}` view
    /// for assertions that do not test the order. `Null` when the list is absent.
    fn perm_view(doc: &serde_json::Value) -> serde_json::Value {
        let Some(rules) = doc.get("permissions").and_then(|v| v.as_array()) else {
            return serde_json::Value::Null;
        };
        let mut view = serde_json::Map::new();
        for rule in rules {
            let action = rule["action"].as_str().unwrap().to_string();
            let entry = view
                .entry(action)
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            entry[rule["resource"].as_str().unwrap()] = rule["effect"].clone();
        }
        for entry in view.values_mut() {
            let only_wildcard = entry
                .as_object()
                .is_some_and(|m| m.len() == 1 && m.contains_key("*"));
            if only_wildcard {
                *entry = entry["*"].clone();
            }
        }
        serde_json::Value::Object(view)
    }

    mod agents;
    mod hook_table;
    mod permission_list;
    mod props;
    mod server;
    use crate::adapter::skills::{arb_distinct_resolved_mcps, arb_string_map};
    use crate::mcp::resolve::ResolvedMcp;
    use crate::merge::rules::RuleFile;

    const VALID_FRONTMATTER: &str = "---\nname: x\ndescription: y\n---\nbody\n";

    /// #1269: an empty `agents_md` must not leave a 0-byte `AGENTS.md` on disk,
    /// mirroring #1262's fix for the Claude Code adapter's `CLAUDE.md`.
    #[test]
    fn materialize_omits_agents_md_when_there_is_no_content() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        assert!(
            !tmp.path().join("AGENTS.md").exists(),
            "no agents_md must write no AGENTS.md at all"
        );
        assert!(
            !owned.contains(&PathBuf::from("AGENTS.md")),
            "AGENTS.md must be absent from the owned set so a stale copy from a \
             prior render is reconciled away as a ghost"
        );
        assert!(
            owned.contains(&PathBuf::from(OPENCODE_JSON_FILE)),
            "owned must include opencode.json, got: {owned:?}"
        );
    }

    /// #1269: whitespace-only content is as empty as the empty string.
    #[test]
    fn materialize_omits_agents_md_when_content_is_only_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            agents_md: "  \n\t\n".into(),
            ..MergedManifest::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        assert!(
            !tmp.path().join("AGENTS.md").exists(),
            "whitespace-only agents_md must write no AGENTS.md"
        );
    }

    #[test]
    fn materialize_points_schema_field_at_the_local_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["$schema"],
            serde_json::json!(format!("./{OPENCODE_SCHEMA_FILE}"))
        );
    }

    /// #1269 non-regression: real content still lands verbatim and is owned.
    #[test]
    fn materialize_agents_md_content_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            agents_md: "# Test Rules\n\nSome content here.".to_string(),
            ..Default::default()
        };
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert_eq!(content, "# Test Rules\n\nSome content here.");
        assert!(owned.contains(&PathBuf::from("AGENTS.md")));
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
        let instr: Vec<&str> = doc["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let root = std::path::absolute(tmp.path()).unwrap();
        for rel in ["rules/security.md", "rules/style.md"] {
            let want = root.join(rel).to_string_lossy().into_owned();
            assert!(instr.contains(&want.as_str()), "{want} missing: {instr:?}");
        }
        assert!(
            instr.iter().all(|i| std::path::Path::new(i).is_absolute()),
            "opencode finds a relative entry from the session folder: {instr:?}"
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
    fn materialize_llmenv_skill_when_task_tracker_enabled() {
        let out = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.features = Some(crate::config::Features {
            task_tracker: Some(crate::config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        OpencodeAdapter.materialize(&manifest, out.path()).unwrap();
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
        OpencodeAdapter
            .materialize(&MergedManifest::default(), out.path())
            .unwrap();
        assert!(!out.path().join("skills/llmenv").exists());
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
    fn materialize_output_style_colliding_with_plugin_skill_is_rejected() {
        // #1333: an output style named after a skill a plugin projects must
        // be rejected before either is written — opencode writes output
        // styles *before* plugin skill projection, so an unrejected
        // collision would silently drop the style once the plugin skill
        // landed on top of it.
        let out = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("skills/my-plugin-skill")).unwrap();
        std::fs::write(
            plugin_dir.path().join("skills/my-plugin-skill/SKILL.md"),
            VALID_FRONTMATTER,
        )
        .unwrap();

        let mut manifest = MergedManifest {
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
        manifest
            .capabilities
            .output_styles
            .push(crate::config::OutputStyle {
                name: "my-plugin-skill".into(),
                description: "A conflicting style".into(),
                content: "Be terse.".into(),
                when: Vec::new(),
                keep_coding_instructions: false,
                force_for_plugin: false,
            });
        let err = OpencodeAdapter
            .materialize(&manifest, out.path())
            .unwrap_err();
        assert!(err.to_string().contains("my-plugin-skill"));
        assert!(
            !out.path().join("skills/my-plugin-skill/SKILL.md").exists(),
            "materialize must bail before writing either the style or the plugin skill"
        );
    }

    // #2356: opencode has no `alwaysLoad`, so the setting changes nothing in its output.
    #[test]
    fn always_load_does_not_change_the_opencode_output() {
        let render = |always_load: Option<bool>| {
            let tmp = tempfile::tempdir().unwrap();
            let mut manifest = MergedManifest::default();
            manifest.mcps.push(ResolvedMcp {
                always_load,
                name: "srv".into(),
                kind: super::ResolvedKind::Stdio {
                    command: "npx".into(),
                    args: vec![],
                    env: std::collections::BTreeMap::new(),
                },
                headers: std::collections::BTreeMap::new(),
                timeout: None,
                disabled_tools: vec![],
                mcp_permissions: None,
                memory_hook: None,
            });
            OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
            std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap()
        };
        assert_eq!(render(Some(true)), render(None));
    }

    #[test]
    fn materialize_mcp_local_server_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(ResolvedMcp {
            always_load: None,
            name: "local-srv".into(),
            kind: super::ResolvedKind::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@anthropic-ai/mcp-server".into()],
                env: std::collections::BTreeMap::from([("FOO".into(), "bar".into())]),
            },
            headers: std::collections::BTreeMap::new(),
            timeout: Some(10_000),
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
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
            always_load: None,
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
            mcp_permissions: None,
            memory_hook: None,
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
            always_load: None,
            name: "minimal".into(),
            kind: super::ResolvedKind::Remote {
                url: "http://example.com".into(),
                transport: crate::config::McpTransport::Http,
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
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
    fn materialize_model_provider_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .model_providers
            .push(llmenv_config::ModelProvider {
                id: "ollama".into(),
                name: Some("Ollama (local)".into()),
                base_url: Some("http://localhost:11434/v1".into()),
                api_type: Some("openai".into()),
                api_key: Some("$OLLAMA_API_KEY".into()),
                headers: std::collections::BTreeMap::from([("X-Custom".into(), "value".into())]),
                models: vec![llmenv_config::ModelSource {
                    id: "llama3.1:70b".into(),
                    name: Some("Llama 3.1 70B".into()),
                    reasoning: true,
                    context_window: Some(128_000),
                    max_tokens: Some(8192),
                    cost: Some(llmenv_config::ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: None,
                        cache_write: None,
                    }),
                    modalities: vec!["text".into()],
                }],
                ..Default::default()
            });
        manifest.capabilities.default_models.insert(
            "large".into(),
            llmenv_config::ModelRef {
                provider: "ollama".into(),
                model: "llama3.1:70b".into(),
            },
        );
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let provider = &doc["provider"]["ollama"];
        assert_eq!(
            provider["npm"],
            serde_json::json!("@ai-sdk/openai-compatible")
        );
        assert_eq!(provider["name"], serde_json::json!("Ollama (local)"));
        assert_eq!(
            provider["options"]["baseURL"],
            serde_json::json!("http://localhost:11434/v1")
        );
        assert_eq!(
            provider["options"]["apiKey"],
            serde_json::json!("$OLLAMA_API_KEY")
        );
        assert_eq!(
            provider["options"]["headers"]["X-Custom"],
            serde_json::json!("value")
        );

        let model = &provider["models"]["llama3.1:70b"];
        assert_eq!(model["name"], serde_json::json!("Llama 3.1 70B"));
        // opencode 2.x rejects a model `reasoning` field, so llmenv does not emit one (#2847).
        assert!(model.get("reasoning").is_none());
        assert_eq!(model["limit"]["context"], serde_json::json!(128_000));
        assert_eq!(model["limit"]["output"], serde_json::json!(8192));
        assert_eq!(model["cost"]["input"], serde_json::json!(0.0));
        assert_eq!(model["modalities"]["input"], serde_json::json!(["text"]));

        assert_eq!(doc["model"], serde_json::json!("ollama/llama3.1:70b"));
        assert!(doc.get("small_model").is_none());
    }

    #[test]
    fn materialize_model_provider_disabled_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .model_providers
            .push(llmenv_config::ModelProvider {
                id: "disabled-provider".into(),
                disabled: true,
                ..Default::default()
            });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("provider").is_none(),
            "\"provider\" key must be absent when all providers are disabled"
        );
    }

    // ── materialize: native_model_providers.opencode merged into provider (#1008) ──

    /// Attach a `native_model_providers.opencode` fragment to `manifest`.
    fn with_native_providers(mut manifest: MergedManifest, yaml: &str) -> MergedManifest {
        manifest
            .capabilities
            .native_model_providers
            .insert("opencode".into(), serde_yaml::from_str(yaml).unwrap());
        manifest
    }

    fn manifest_with_mtplx_provider() -> MergedManifest {
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .model_providers
            .push(llmenv_config::ModelProvider {
                id: "mtplx".into(),
                name: Some("modeled".into()),
                base_url: Some("http://localhost:8080/v1".into()),
                models: vec![llmenv_config::ModelSource {
                    id: "gpt-oss".into(),
                    ..Default::default()
                }],
                ..Default::default()
            });
        manifest
    }

    fn materialized_opencode_doc(manifest: &MergedManifest) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        OpencodeAdapter.materialize(manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn materialize_native_model_providers_without_modeled_providers() {
        // No `model_providers` at all: the fragment alone must still render a
        // `provider` block — otherwise the escape hatch is unusable on its own.
        let doc = materialized_opencode_doc(&with_native_providers(
            MergedManifest::default(),
            "mtplx:\n  npm: '@ai-sdk/openai-compatible'\n  \
             options:\n    baseURL: http://localhost:8080/v1\n",
        ));
        assert_eq!(
            doc["provider"]["mtplx"]["options"]["baseURL"],
            serde_json::json!("http://localhost:8080/v1")
        );
    }

    #[test]
    fn materialize_native_model_providers_deep_merges_onto_rendered_provider() {
        let doc = materialized_opencode_doc(&with_native_providers(
            manifest_with_mtplx_provider(),
            "mtplx:\n  models:\n    gpt-oss:\n      reasoningEffort: high\n",
        ));
        assert_eq!(
            doc["provider"]["mtplx"]["models"]["gpt-oss"]["reasoningEffort"],
            serde_json::json!("high"),
            "unmodeled per-model key must be injected"
        );
        assert_eq!(
            doc["provider"]["mtplx"]["options"]["baseURL"],
            serde_json::json!("http://localhost:8080/v1"),
            "deep merge must preserve sibling keys rendered from model_providers"
        );
    }

    #[test]
    fn materialize_native_model_providers_empty_mapping_omits_provider_key() {
        let doc =
            materialized_opencode_doc(&with_native_providers(MergedManifest::default(), "{}"));
        assert!(
            doc.get("provider").is_none(),
            "an empty fragment must not emit an empty \"provider\" object"
        );
    }

    /// A non-mapping fragment must be rejected, not silently swallow the whole
    /// rendered provider block (which is what `merge_json` would do).
    #[test]
    fn materialize_native_model_providers_non_mapping_errors() {
        for (yaml, kind) in [
            ("oops", "a string"),
            ("~", "null"),
            ("[a, b]", "a sequence"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let manifest = with_native_providers(manifest_with_mtplx_provider(), yaml);
            let err = OpencodeAdapter
                .materialize(&manifest, tmp.path())
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("native_model_providers.opencode") && msg.contains(kind),
                "error must name the field and the actual shape, got: {msg}"
            );
        }
    }

    #[test]
    fn materialize_model_provider_empty_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc.get("provider").is_none());
        assert!(doc.get("model").is_none());
        assert!(doc.get("small_model").is_none());
    }

    #[test]
    fn materialize_model_provider_limit_omitted_without_both_bounds() {
        // opencode's schema requires both `context` and `output` when `limit`
        // is present at all — a model with only one of the two must omit
        // `limit` entirely rather than emit a half-populated object.
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .model_providers
            .push(llmenv_config::ModelProvider {
                id: "p".into(),
                models: vec![llmenv_config::ModelSource {
                    id: "m".into(),
                    context_window: Some(128_000),
                    max_tokens: None,
                    ..Default::default()
                }],
                ..Default::default()
            });
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc["provider"]["p"]["models"]["m"].get("limit").is_none());
    }

    #[test]
    fn materialize_default_models_unknown_role_dropped() {
        // opencode only has two default-model slots (model/small_model); a
        // role name it doesn't recognize has nowhere to go and must not
        // error or silently invent a field.
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.default_models.insert(
            "coding".into(),
            llmenv_config::ModelRef {
                provider: "anthropic".into(),
                model: "claude-opus-4-5".into(),
            },
        );
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc.get("model").is_none());
        assert!(doc.get("small_model").is_none());
        assert!(doc.get("coding").is_none());
    }

    #[test]
    fn materialize_native_opencode_rejects_provider_key() {
        // `provider`/`model`/`small_model` are modeled keys — a
        // `native.opencode` fragment must not be allowed to clobber them.
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        let frag: serde_yaml::Value = serde_yaml::from_str("provider:\n  evil: {}\n").unwrap();
        manifest.native.insert("opencode".into(), frag);
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(err.to_string().contains("provider"), "{err}");
    }

    /// #1270: `native.opencode: {$schema: null}` must delete a key the render
    /// already emitted, mirroring #1264's fix for the Claude Code adapter's
    /// `settings.json`. `$schema` is always rendered and is not in
    /// `OPENCODE_MODELED_KEYS`, so the catch-all accepts overriding it.
    #[test]
    fn materialize_native_null_removes_a_rendered_opencode_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        let frag: serde_yaml::Value = serde_yaml::from_str("$schema: null").unwrap();
        manifest.native.insert("opencode".into(), frag);
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc.get("$schema").is_none(),
            "`native.opencode.$schema: null` must delete the key, got: {doc}"
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

    /// #1130: opencode has no native output-style concept, so a declared
    /// style falls back to a generated skill.
    #[test]
    fn materialize_output_style_falls_back_to_generated_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.output_styles.push(crate::config::OutputStyle {
            name: "concise".into(),
            description: "A test style".into(),
            content: "Be terse.".into(),
            when: Vec::new(),
            keep_coding_instructions: false,
            force_for_plugin: false,
        });
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let skill_md = tmp.path().join("skills/concise/SKILL.md");
        assert!(skill_md.exists());
        assert!(owned.contains(&PathBuf::from("skills/concise/SKILL.md")));
        let content = std::fs::read_to_string(skill_md).unwrap();
        assert!(content.contains("Be terse."));
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
        // perm_view(&doc)["shell"] = "allow"
        assert_eq!(perm_view(&doc)["shell"], serde_json::json!("allow"));
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
        // Old format: perm_view(&doc)["allow"] as array with "Bash(echo*)"
        // New format: perm_view(&doc)["shell"]["echo*"] = "allow"
        let bash = &perm_view(&doc)["shell"];
        assert_eq!(bash["echo*"], serde_json::json!("allow"));
    }

    /// #1326 (security-audit): `native_permissions.opencode` is the escape
    /// hatch for authoring opencode's *own* native vocabulary — keys like
    /// `lsp`/`skill`/`external_directory` and a bare `"*"` deny-all baseline
    /// have no neutral (Claude Code) equivalent at all. An earlier version
    /// of this fix incorrectly ran native tool strings through the same
    /// restrictive neutral-name mapping `rule_to_patterns` uses, which
    /// silently dropped every native rule outside the twelve mapped
    /// PascalCase names — including a working `deny: ["*"]` baseline,
    /// turning it into a permissive default. Native rules must only be
    /// lowercased, never mapped.
    #[test]
    fn materialize_native_opencode_preserves_opencode_only_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec![],
                ask: vec![],
                deny: vec!["*".into(), "lsp".into()],
            },
        );
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            perm_view(&doc)["*"],
            serde_json::json!("deny"),
            "a bare `*` native deny-all rule must survive verbatim, not be dropped as an \
             unmapped neutral tool: {doc}"
        );
        assert_eq!(
            perm_view(&doc)["lsp"],
            serde_json::json!("deny"),
            "opencode-only keys like `lsp` (no neutral equivalent) must survive verbatim: {doc}"
        );
    }

    /// #1326: source-verified against opencode's own
    /// `packages/core/src/v1/config/permission.ts` schema — the real
    /// permission keys are `read, edit, glob, grep, list, bash, task,
    /// todowrite, webfetch, websearch, lsp, skill`, not a simple lowercase of
    /// Claude Code's tool names. `write`/`patch` are explicitly normalized to
    /// `edit` (`migrate.ts`'s `normalizeAction`) — opencode has no separate
    /// `write`/`multiedit` key — and the list-tool key is `list`, not `ls`.
    #[test]
    fn materialize_permissions_map_write_and_multiedit_to_edit_and_drop_ls() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        for tool in ["Write", "MultiEdit", "LS"] {
            caps.permissions.allow.push(crate::config::PermissionRule {
                tool: tool.into(),
                pattern: None,
                paths: vec![],
            });
        }
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            perm_view(&doc)["edit"],
            serde_json::json!("allow"),
            "Write and MultiEdit must both map to opencode's `edit` key, got: {doc}"
        );
        // opencode 2.x has no list tool, so an LS rule is dropped (#2846).
        assert!(perm_view(&doc).get("list").is_none(), "got: {doc}");
        assert!(perm_view(&doc).get("write").is_none());
        assert!(perm_view(&doc).get("multiedit").is_none());
        assert!(perm_view(&doc).get("ls").is_none());
    }

    /// #1326: a neutral tool with no confirmed opencode equivalent (e.g.
    /// `NotebookEdit`) is dropped, not silently rendered as an unverified
    /// lowercase guess — matches the design doc's "unknown names dropped
    /// with warning" policy and the Crush adapter's equivalent handling.
    #[test]
    fn materialize_permissions_unmapped_tool_is_dropped_not_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "NotebookEdit".into(),
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
        assert!(
            doc.get("permission").is_none(),
            "an unmapped tool must be dropped, not fail materialize or render a guessed \
             key: {doc}"
        );
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
        let bash = &perm_view(&doc)["shell"];
        assert_eq!(bash["otool *"], serde_json::json!("allow"));
        assert_eq!(bash["rm *"], serde_json::json!("deny"));

        // Bare tools without patterns -> emitted as action string directly
        assert_eq!(perm_view(&doc)["read"], serde_json::json!("allow"));
        assert_eq!(perm_view(&doc)["edit"], serde_json::json!("deny"));

        // No flat "allow"/"deny"/"ask" arrays at the permission level
        assert!(perm_view(&doc).get("allow").is_none());
        assert!(perm_view(&doc).get("deny").is_none());
        assert!(perm_view(&doc).get("ask").is_none());
    }

    /// The unscoped form is the only legal shape for an action-only key, and
    /// it must keep working — this is the fix's control case.
    #[test]
    fn materialize_bare_rule_on_action_only_key_still_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "WebFetch".into(),
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
        assert_eq!(perm_view(&doc)["webfetch"], serde_json::json!("deny"));
    }

    /// #1328 gap 2: opencode resolves two patterns that both match a resource
    /// with `findLast` — last key in the JSON wins. llmenv serializes the
    /// pattern map alphabetically, which can place a *broader* pattern after
    /// the narrower one it swallows, silently turning a deny into an allow.
    #[test]
    fn materialize_broader_pattern_sorting_last_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        // "rm -rf*" sorts after "rm -rf /" ('*' > ' '), so opencode's findLast
        // picks the allow and the explicit deny never takes effect.
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: Some("rm -rf*".into()),
            paths: vec![],
        });
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: Some("rm -rf /".into()),
            paths: vec![],
        });
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rm -rf*") && msg.contains("rm -rf /"),
            "error must name both conflicting patterns: {msg}"
        );
    }

    /// The benign shape — a broad rule plus a narrower override — already
    /// sorts correctly ('*' precedes most literals), and must keep rendering.
    #[test]
    fn materialize_broad_rule_with_narrower_override_still_renders() {
        for (broad_action, narrow_action) in [("allow", "deny"), ("deny", "allow")] {
            let tmp = tempfile::tempdir().unwrap();
            let mut caps = crate::config::Capabilities::default();
            let broad = crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: None,
                paths: vec![],
            };
            let narrow = crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: Some("git push*".into()),
                paths: vec![],
            };
            for (action, rule) in [(broad_action, broad), (narrow_action, narrow)] {
                match action {
                    "allow" => caps.permissions.allow.push(rule),
                    _ => caps.permissions.deny.push(rule),
                }
            }
            let manifest = MergedManifest {
                capabilities: caps,
                ..Default::default()
            };
            let rendered = OpencodeAdapter.materialize(&manifest, tmp.path());
            assert!(
                rendered.is_ok(),
                "broad {broad_action} + narrow {narrow_action} must render: {:#}",
                rendered.unwrap_err()
            );
            let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let bash = &perm_view(&doc)["shell"];
            assert_eq!(bash["*"], serde_json::json!(broad_action));
            assert_eq!(bash["git push*"], serde_json::json!(narrow_action));
        }
    }

    /// The idiomatic way to write "deny this flag anywhere" is a leading-`*`
    /// pattern, which sorts before an alphanumeric allow it only partially
    /// overlaps. Neither pattern contains the other, so a containment-only
    /// check misses it while opencode still resolves the shared inputs to the
    /// later-sorting allow.
    #[test]
    fn materialize_partially_overlapping_patterns_are_rejected() {
        for (allow, deny) in [
            ("git *", "* --force*"),
            ("bash *", "* sudo *"),
            ("src/*", "*/secrets/*"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut caps = crate::config::Capabilities::default();
            caps.permissions.allow.push(crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: Some(allow.into()),
                paths: vec![],
            });
            caps.permissions.deny.push(crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: Some(deny.into()),
                paths: vec![],
            });
            let manifest = MergedManifest {
                capabilities: caps,
                ..Default::default()
            };
            let err = OpencodeAdapter
                .materialize(&manifest, tmp.path())
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains(allow) && msg.contains(deny),
                "error must name both overlapping patterns: {msg}"
            );
        }
    }

    /// A native rule whose tool name carries stray whitespace would render a
    /// key opencode can never match — an inert rule, so it has to fail.
    #[test]
    fn materialize_native_rule_with_spaced_tool_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec![],
                ask: vec![],
                deny: vec!["web fetch(https://example.com/*)".into()],
            },
        );
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(format!("{err:#}").contains("whitespace"));
    }

    /// A native rule padded with spaces around the tool name is trimmed rather
    /// than rendered as a dead key.
    #[test]
    fn materialize_native_rule_tool_name_is_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec![],
                ask: vec![],
                deny: vec![" lsp (foo)".into()],
            },
        );
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(perm_view(&doc)["lsp"]["foo"], serde_json::json!("deny"));
    }

    /// #1344: opencode wildcard-matches the permission *key* against the tool
    /// name too, so a rule authored under `*` shadows every concrete key that
    /// sorts after it. A per-key check never compared the two.
    #[test]
    fn materialize_wildcard_key_shadowing_a_concrete_key_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec![],
                ask: vec![],
                deny: vec!["*(git push --force*)".into()],
            },
        );
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: None,
            paths: vec![],
        });
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git push --force*") && msg.contains("shell"),
            "error must name the wildcard rule and the key it shadows: {msg}"
        );
        // Narrowing the allow is not expressible in opencode, so the remedy has
        // to be the one that works: restate the deny on the concrete key.
        assert!(
            msg.contains("`shell(git push --force*)`"),
            "error must suggest moving the rule onto the shadowed key: {msg}"
        );
    }

    /// The documented deny-all baseline — a bare native `*` plus a narrower
    /// per-tool allow — is narrower in both dimensions, so it must still render.
    #[test]
    fn materialize_wildcard_key_deny_all_baseline_still_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec![],
                ask: vec![],
                deny: vec!["*".into()],
            },
        );
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
        assert_eq!(perm_view(&doc)["*"], serde_json::json!("deny"));
        assert_eq!(perm_view(&doc)["shell"], serde_json::json!("allow"));
    }

    /// Two concrete keys are distinct literals, so they never overlap and must
    /// never be flagged against each other.
    #[test]
    fn materialize_distinct_concrete_keys_do_not_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.allow.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: None,
            paths: vec![],
        });
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "Read".into(),
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
        assert_eq!(perm_view(&doc)["shell"], serde_json::json!("allow"));
        assert_eq!(perm_view(&doc)["read"], serde_json::json!("deny"));
    }

    /// #1345: opencode has a real `skill` permission key, so a `Skill` rule has
    /// an exact equivalent and must not be dropped.
    #[test]
    fn materialize_skill_rule_maps_to_the_skill_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "Skill".into(),
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
        assert_eq!(perm_view(&doc)["skill"], serde_json::json!("deny"));
    }

    #[test]
    fn patterns_overlap_matches_shared_inputs() {
        // Disjoint prefixes never share an input.
        assert!(!patterns_overlap("otool *", "rm *"));
        assert!(!patterns_overlap("git push*", "git pull*"));
        // Partial overlap: neither contains the other, but both match
        // "git push --force".
        assert!(patterns_overlap("git *", "* --force*"));
        assert!(patterns_overlap("a*", "*b"));
        // Containment is a special case of overlap.
        assert!(patterns_overlap("*", "anything"));
        assert!(patterns_overlap("git *", "git push*"));
        // The trailing " *" spelling also matches the bare prefix.
        assert!(patterns_overlap("git *", "git"));
        assert!(!patterns_overlap("git *", "gitk"));
        // `?` spans exactly one character.
        assert!(patterns_overlap("a?c", "abc"));
        assert!(!patterns_overlap("a?c", "abbc"));
    }

    #[test]
    fn wildcard_matches_mirrors_opencode_semantics() {
        // `*` spans anything, including separators.
        assert!(wildcard_matches("*", ""));
        assert!(wildcard_matches("*", "anything at all"));
        assert!(wildcard_matches("rm -rf*", "rm -rf /"));
        assert!(wildcard_matches("a*c", "abbbc"));
        assert!(!wildcard_matches("a*c", "abbb"));
        // `?` spans exactly one character.
        assert!(wildcard_matches("a?c", "abc"));
        assert!(!wildcard_matches("a?c", "ac"));
        assert!(!wildcard_matches("a?c", "abbc"));
        // A pattern ending in " *" also matches the bare prefix.
        assert!(wildcard_matches("git *", "git"));
        assert!(wildcard_matches("git *", "git status"));
        assert!(!wildcard_matches("git *", "gitk"));
        // Backslashes normalize to forward slashes on both sides.
        assert!(wildcard_matches("src/*", "src\\main.rs"));
        // Anchored at both ends.
        assert!(!wildcard_matches("rm", "rm -rf"));
        assert!(!wildcard_matches("rf", "rm -rf"));
        // Regex metacharacters are literal.
        assert!(wildcard_matches("a.c", "a.c"));
        assert!(!wildcard_matches("a.c", "abc"));
        assert!(wildcard_matches("a+b", "a+b"));
    }

    #[test]
    fn pattern_subsumes_detects_containment_directionally() {
        assert!(pattern_subsumes("rm -rf*", "rm -rf /"));
        assert!(!pattern_subsumes("rm -rf /", "rm -rf*"));
        assert!(pattern_subsumes("*", "git push origin main"));
        assert!(!pattern_subsumes("git *", "*"));
        assert!(pattern_subsumes("git *", "git push*"));
        assert!(!pattern_subsumes("git push*", "git *"));
        // Disjoint patterns subsume in neither direction.
        assert!(!pattern_subsumes("otool *", "rm *"));
        assert!(!pattern_subsumes("rm *", "otool *"));
        // A wildcard is never subsumed by a literal that merely matches it.
        assert!(!pattern_subsumes("a", "a*"));
    }

    proptest! {
        /// A pattern always subsumes itself, so an equal pair can never be
        /// reported as an ordering inversion.
        #[test]
        fn pattern_subsumes_is_reflexive(pattern in "[a-z /*?-]{0,12}") {
            prop_assert!(pattern_subsumes(&pattern, &pattern));
        }

        /// `*` matches every input, so it subsumes every other pattern.
        #[test]
        fn star_subsumes_everything(pattern in "[a-z /*?-]{0,12}") {
            prop_assert!(pattern_subsumes("*", &pattern));
        }

        /// Overlap is symmetric, and a pattern always overlaps itself.
        #[test]
        fn overlap_is_symmetric_and_reflexive(a in "[ab */?]{0,6}", b in "[ab */?]{0,6}") {
            prop_assert_eq!(patterns_overlap(&a, &b), patterns_overlap(&b, &a));
            prop_assert!(patterns_overlap(&a, &a));
        }

        /// Adversarial star runs must not panic or hang.
        #[test]
        fn glob_matches_terminates_on_star_runs(
            pattern in "[*?ab]{0,16}",
            input in "[ab]{0,16}",
        ) {
            let chars: Vec<char> = input.chars().collect();
            let _ = glob_matches(&pattern, &chars);
        }
    }

    /// Resolve a probe the way opencode does: flatten every key in rendered
    /// order, keep the rules whose key matches the tool and whose pattern
    /// matches the resource, and take the last one (`findLast`).
    fn opencode_resolution<'a>(
        map: &'a BTreeMap<String, BTreeMap<String, String>>,
        tool: &str,
        resource: &str,
    ) -> Vec<(&'a str, &'a str, &'a str)> {
        map.iter()
            .flat_map(|(key, patterns)| {
                patterns
                    .iter()
                    .map(move |(p, a)| (key.as_str(), p.as_str(), a.as_str()))
            })
            .filter(|(key, pattern, _)| {
                wildcard_matches(key, tool) && wildcard_matches(pattern, resource)
            })
            .collect()
    }

    proptest! {
        /// End-to-end oracle for the ordering check. For any permission map the
        /// check *accepts*, replay opencode's own resolution over a probe: the
        /// rule that wins must either agree with every other rule that matched,
        /// or be narrower than it in both key and pattern. Anything else is an
        /// inversion the check should have rejected.
        #[test]
        fn accepted_maps_never_let_a_broader_rule_win(
            rules in proptest::collection::vec(
                (
                    "(\\*|bash|read|b\\*)",
                    "(\\*|a\\*|ab|\\*b|a\\?|b)",
                    "(allow|deny)",
                ),
                1..5,
            ),
            tool in "(bash|read)",
            resource in "[ab]{0,3}",
        ) {
            let mut map: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            for (key, pattern, action) in rules {
                map.entry(key).or_default().insert(pattern, action);
            }
            prop_assume!(check_rule_ordering(&map).is_ok());

            let matched = opencode_resolution(&map, &tool, &resource);
            let Some(winner) = matched.last() else { return Ok(()) };
            for other in &matched {
                if other.2 == winner.2 {
                    continue;
                }
                prop_assert!(
                    pattern_subsumes(other.0, winner.0) && pattern_subsumes(other.1, winner.1),
                    "opencode would resolve ({tool:?}, {resource:?}) to {winner:?}, which is \
                     not narrower than the also-matching {other:?} — the check accepted an \
                     inverted map"
                );
            }
        }
    }

    /// Every string over `alphabet` up to `max_len`, shortest first.
    fn all_strings(alphabet: &[char], max_len: usize) -> Vec<String> {
        let mut out = vec![String::new()];
        let mut frontier = vec![String::new()];
        for _ in 0..max_len {
            let mut next = Vec::new();
            for base in &frontier {
                for c in alphabet {
                    let mut s = base.clone();
                    s.push(*c);
                    next.push(s);
                }
            }
            out.extend(next.iter().cloned());
            frontier = next;
        }
        out
    }

    /// The invariant the ordering check's exemption rests on: when `broad` is
    /// treated as covering `narrow`, everything matching `narrow` must really
    /// match `broad`. A violation would silently exempt a pair that does invert
    /// — so this is checked exhaustively rather than sampled.
    #[test]
    fn subsumption_never_exempts_a_pattern_it_does_not_cover() {
        let patterns = all_strings(&['a', 'b', '*', '?'], 3);
        let inputs = all_strings(&['a', 'b'], 4);
        for broad in &patterns {
            for narrow in &patterns {
                if !pattern_subsumes(broad, narrow) {
                    continue;
                }
                for input in &inputs {
                    assert!(
                        !wildcard_matches(narrow, input) || wildcard_matches(broad, input),
                        "{broad:?} was treated as covering {narrow:?}, but {input:?} \
                         matches only {narrow:?}"
                    );
                }
            }
        }
    }

    /// A concrete shared input is proof of overlap, so the DP must never call
    /// such a pair disjoint — a false "disjoint" skips the ordering check.
    #[test]
    fn shared_input_is_never_reported_disjoint() {
        let patterns = all_strings(&['a', 'b', '*', '?'], 3);
        let inputs = all_strings(&['a', 'b'], 4);
        for a in &patterns {
            for b in &patterns {
                if patterns_overlap(a, b) {
                    continue;
                }
                for input in &inputs {
                    assert!(
                        !(wildcard_matches(a, input) && wildcard_matches(b, input)),
                        "{input:?} matches both {a:?} and {b:?} but they were reported disjoint"
                    );
                }
            }
        }
    }

    /// Two overlapping patterns that agree on the action are order-independent
    /// — no diagnostic, no rejection.
    #[test]
    fn materialize_overlapping_patterns_with_same_action_are_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        for pattern in ["rm -rf*", "rm -rf /"] {
            caps.permissions.deny.push(crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: Some(pattern.into()),
                paths: vec![],
            });
        }
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            perm_view(&doc)["shell"]["rm -rf /"],
            serde_json::json!("deny")
        );
    }

    #[test]
    fn materialize_native_allow_does_not_override_structured_deny() {
        // Regression test for #877: a native `allow` for the same tool+pattern
        // as a structured `deny` must not win — deny is the safe-by-default
        // tier and must always win regardless of which source wrote it.
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        caps.permissions.deny.push(crate::config::PermissionRule {
            tool: "Bash".into(),
            pattern: Some("rm *".into()),
            paths: vec![],
        });
        caps.native_permissions.insert(
            "opencode".into(),
            crate::config::NativePermissionRules {
                allow: vec!["Bash(rm *)".into()],
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
        let bash = &perm_view(&doc)["shell"];
        assert_eq!(
            bash["rm *"],
            serde_json::json!("deny"),
            "structured deny must win over native allow for the same tool+pattern"
        );
    }

    /// Materialize a manifest whose `native_permissions.opencode` carries a
    /// single rule string in the given tier ("allow"/"ask"/"deny"), and
    /// return the resulting error message.
    fn native_rule_error(tier: &str, rule: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let mut caps = crate::config::Capabilities::default();
        let mut native = crate::config::NativePermissionRules::default();
        if tier == "ask" {
            native.ask.push(rule.to_string());
        } else if tier == "deny" {
            native.deny.push(rule.to_string());
        } else {
            native.allow.push(rule.to_string());
        }
        caps.native_permissions.insert("opencode".into(), native);
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn materialize_rejects_malformed_native_rules() {
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "allow",
                "Bash(",
                &["Bash(", "native_permissions.opencode.allow"],
            ),
            (
                "ask",
                "Bash)",
                &["Bash)", "native_permissions.opencode.ask"],
            ),
            ("allow", "(rm *)", &["(rm *)"]),
            (
                "deny",
                "Bash()",
                &["Bash()", "native_permissions.opencode.deny"],
            ),
            ("allow", "", &["native_permissions.opencode.allow"]),
        ];
        for (tier, rule, needles) in cases {
            let msg = native_rule_error(tier, rule);
            for needle in *needles {
                assert!(
                    msg.contains(needle),
                    "rule {rule:?} in {tier}: expected {needle:?} in error: {msg}"
                );
            }
        }
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
    fn translate_command_md_keeps_description() {
        let src = "---\ndescription: My test command\nmodel: claude-sonnet-4-20250514\nallowed_tools: [Bash]\n---\n\nRun this command.";
        let result = translate_command_md(src).unwrap();
        assert!(result.contains("description: My test command"));
        assert!(!result.contains("model:"));
        assert!(!result.contains("allowed_tools:"));
        assert!(result.contains("Run this command."));
    }

    #[test]
    fn translate_command_md_no_frontmatter_passthrough() {
        let src = "Just raw markdown content.";
        let result = translate_command_md(src).unwrap();
        assert_eq!(result, "Just raw markdown content.");
    }

    #[test]
    fn translate_command_md_empty_description_still_works() {
        let src = "---\n---\n\nBody only.";
        let result = translate_command_md(src).unwrap();
        // No valid frontmatter fields → body returned as-is (no --- wrapper)
        assert_eq!(result, "Body only.");
    }

    #[test]
    fn translate_agent_md_keeps_model_description_tools() {
        let src = "---\ndescription: My agent\nmodel: claude-sonnet-4-20250514\ntools: [Bash, Read]\n---\n\nDo things autonomously.";
        let result = translate_agent_md(src).unwrap();
        assert!(result.contains("description: My agent"));
        assert!(result.contains("model: claude-sonnet-4-20250514"));
        assert!(result.contains("tools:"));
        assert!(result.contains("mode: subagent"));
        assert!(result.contains("Do things autonomously."));
    }

    #[test]
    fn translate_agent_md_adds_subagent_mode() {
        let src = "---\ndescription: Just description\n---\n\nBody.";
        let result = translate_agent_md(src).unwrap();
        assert!(result.contains("description: Just description"));
        assert!(result.contains("mode: subagent"));
    }

    #[test]
    fn translate_agent_md_no_frontmatter_passthrough() {
        let src = "Just raw content.";
        let result = translate_agent_md(src).unwrap();
        assert!(result.contains("mode: subagent"));
        assert!(result.contains("Just raw content."));
    }

    #[test]
    fn translate_agent_md_accepts_allowed_tools() {
        let src = "---\ndescription: Agent with allowed tools\nallowed_tools: [Bash]\n---\n\nBody.";
        let result = translate_agent_md(src).unwrap();
        assert!(result.contains("allowed_tools:"));
        assert!(result.contains("mode: subagent"));
    }

    #[test]
    fn translate_agent_md_drops_unsupported_keys() {
        let src = "---\ndescription: My agent\nmodel: claude-sonnet-4-20250514\nomitClaudeMd: true\neffort: high\nmaxTurns: 5\n---\n\nBody.";
        let result = translate_agent_md(src).unwrap();
        // Supported keys should be present
        assert!(result.contains("description: My agent"));
        assert!(result.contains("model: claude-sonnet-4-20250514"));
        assert!(result.contains("mode: subagent"));
        // Dropped keys should not be present
        assert!(!result.contains("omitClaudeMd"));
        assert!(!result.contains("effort"));
        assert!(!result.contains("maxTurns"));
    }

    #[test]
    fn dropped_agent_keys_lists_unsupported_keys_in_source_order() {
        let map: serde_yaml::Mapping = serde_yaml::from_str(
            "name: a\ndescription: d\nomitClaudeMd: true\nmodel: m\neffort: high\n\
             tools: [Bash]\npermissionMode: plan\n",
        )
        .unwrap();
        assert_eq!(
            dropped_agent_keys(&map),
            ["omitClaudeMd", "effort", "permissionMode"]
        );
    }

    #[test]
    fn dropped_agent_keys_reports_non_string_keys() {
        let map: serde_yaml::Mapping =
            serde_yaml::from_str("description: d\n1: x\ntrue: y\n").unwrap();
        assert_eq!(dropped_agent_keys(&map), ["1", "true"]);
    }

    proptest::proptest! {
        /// The output is exactly the source keys outside the allowlist, in source order.
        #[test]
        fn dropped_agent_keys_is_the_ordered_complement_of_the_allowlist(
            keys in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::sample::select(OPENCODE_AGENT_KEYS.to_vec()).prop_map(String::from),
                    "[a-zA-Z]{1,10}",
                ],
                0..10,
            ),
        ) {
            use proptest::prelude::*;
            let mut map = serde_yaml::Mapping::new();
            for k in &keys {
                map.insert(serde_yaml::Value::from(k.as_str()), serde_yaml::Value::from(1));
            }
            let expected: Vec<String> = map
                .keys()
                .filter_map(serde_yaml::Value::as_str)
                .filter(|k| !OPENCODE_AGENT_KEYS.contains(k))
                .map(String::from)
                .collect();
            prop_assert_eq!(dropped_agent_keys(&map), expected);
        }
    }

    #[test]
    fn dropped_agent_keys_is_empty_for_supported_keys_only() {
        let map: serde_yaml::Mapping =
            serde_yaml::from_str("name: a\ndescription: d\nallowed_tools: [Read]\n").unwrap();
        assert!(dropped_agent_keys(&map).is_empty());
    }

    #[test]
    fn trait_capability_flags() {
        assert!(OpencodeAdapter.supports_plugins());
        assert!(OpencodeAdapter.supports_lsp());
        assert!(OpencodeAdapter.supports_model_providers());
        assert!(
            OpencodeAdapter
                .supported_hook_events()
                .contains(&"PreToolUse")
        );
        // The shim reads hookSpecificOutput.additionalContext on SessionStart.
        let out = OpencodeAdapter.emit_hook_context("SessionStart", "hi", "");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            "[ICM MEMORY CONTEXT (session start)]\nhi"
        );
    }

    fn plugin_manifest(
        plugin_dir: &Path,
    ) -> (
        Vec<crate::plugins::resolve::ResolvedPlugin>,
        Vec<crate::plugins::resolve::ResolvedMarketplace>,
    ) {
        (
            vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: "test".into(),
                plugin: "my-plugin".into(),
                collection: String::new(),
                install_path: Some(plugin_dir.to_str().unwrap().into()),
                git_commit_sha: None,
            }],
            vec![crate::plugins::resolve::ResolvedMarketplace {
                name: "test".into(),
                source: String::new(),
                install_location: None,
                head: None,
            }],
        )
    }

    #[test]
    fn materialize_rules_path_traversal_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.rules.push(RuleFile {
            bundle: "test".into(),
            rel: PathBuf::from("../escape.md"),
            frontmatter: None,
            body: "evil".into(),
            raw: "evil".into(),
        });
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(err.to_string().contains("path traversal"));
    }

    #[test]
    fn parse_plugin_hooks_happy_and_warning_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [{
                        "hooks": [
                            {"type": "command", "command": "echo good"},
                            {"type": "prompt", "command": "echo bad-kind"},
                            {"type": "command"},
                        ]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();
        let hooks = parse_plugin_hooks(&hooks_path, tmp.path(), "my-plugin")
            .unwrap()
            .unwrap();
        assert_eq!(hooks.len(), 1, "only the valid command hook must survive");
        assert_eq!(hooks[0].handler.command.as_deref(), Some("echo good"));
    }

    #[test]
    fn parse_plugin_mcp_entries_reads_valid_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let mcp_path = tmp.path().join("LLM_PROVIDER_MCP_JSON");
        std::fs::write(&mcp_path, r#"{"srv": {"type": "local", "command": ["x"]}}"#).unwrap();
        let entries = parse_plugin_mcp_entries(&mcp_path, "my-plugin").unwrap();
        assert!(entries.contains_key("srv"));
    }

    #[test]
    fn parse_plugin_mcp_entries_rejects_non_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let mcp_path = tmp.path().join("LLM_PROVIDER_MCP_JSON");
        std::fs::write(&mcp_path, "- not\n- a\n- mapping\n").unwrap();
        let err = parse_plugin_mcp_entries(&mcp_path, "my-plugin").unwrap_err();
        assert!(err.to_string().contains("must be a mapping"));
    }

    #[test]
    fn materialize_plugin_mcp_entries_written_and_deduped() {
        let out = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            plugin_dir.path().join("LLM_PROVIDER_MCP_JSON"),
            r#"{"shared": {"type": "local", "command": ["from-plugin"]}}"#,
        )
        .unwrap();
        let (plugins, marketplaces) = plugin_manifest(plugin_dir.path());
        let mut manifest = MergedManifest {
            plugins,
            marketplaces,
            ..Default::default()
        };
        manifest.mcps.push(ResolvedMcp {
            always_load: None,
            name: "shared".into(),
            kind: super::ResolvedKind::Remote {
                url: "http://example.com".into(),
                transport: crate::config::McpTransport::Http,
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
        });
        OpencodeAdapter.materialize(&manifest, out.path()).unwrap();
        let raw = std::fs::read_to_string(out.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Plugin entry (last-write-wins) must have overridden the manifest MCP.
        assert_eq!(
            doc["mcp"]["shared"]["command"],
            serde_json::json!(["from-plugin"])
        );
    }

    #[test]
    fn materialize_plugin_mcp_bad_shape_skipped_without_aborting() {
        let out = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            plugin_dir.path().join("LLM_PROVIDER_MCP_JSON"),
            "- not\n- a\n- mapping\n",
        )
        .unwrap();
        let (plugins, marketplaces) = plugin_manifest(plugin_dir.path());
        let manifest = MergedManifest {
            plugins,
            marketplaces,
            ..Default::default()
        };
        // Must not abort materialize — just skip this plugin's MCP servers.
        OpencodeAdapter.materialize(&manifest, out.path()).unwrap();
    }

    #[test]
    fn materialize_plugin_hooks_parse_error_skips_only_that_plugin() {
        let out = tempfile::tempdir().unwrap();
        let bad_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(bad_dir.path().join("hooks")).unwrap();
        std::fs::write(bad_dir.path().join("hooks/hooks.json"), "not json").unwrap();

        let good_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(good_dir.path().join("commands")).unwrap();
        std::fs::write(
            good_dir.path().join("commands/still-works.md"),
            "---\ndescription: still works\n---\nbody\n",
        )
        .unwrap();

        let manifest = MergedManifest {
            plugins: vec![
                crate::plugins::resolve::ResolvedPlugin {
                    marketplace: "test".into(),
                    plugin: "bad-plugin".into(),
                    collection: String::new(),
                    install_path: Some(bad_dir.path().to_str().unwrap().into()),
                    git_commit_sha: None,
                },
                crate::plugins::resolve::ResolvedPlugin {
                    marketplace: "test".into(),
                    plugin: "good-plugin".into(),
                    collection: String::new(),
                    install_path: Some(good_dir.path().to_str().unwrap().into()),
                    git_commit_sha: None,
                },
            ],
            marketplaces: vec![crate::plugins::resolve::ResolvedMarketplace {
                name: "test".into(),
                source: String::new(),
                install_location: None,
                head: None,
            }],
            ..Default::default()
        };
        // Must succeed overall, and the good plugin's content must still land.
        OpencodeAdapter.materialize(&manifest, out.path()).unwrap();
        assert!(out.path().join("command/__plugin_still-works.md").exists());
    }

    #[test]
    fn materialize_bundle_path_traversal_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest
            .files
            .insert(PathBuf::from("../escape.md"), PathBuf::from("/dev/null"));
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(err.to_string().contains("path traversal"));
    }

    #[test]
    fn materialize_bundle_unreadable_command_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.files.insert(
            PathBuf::from("commands/missing.md"),
            PathBuf::from("/does/not/exist.md"),
        );
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(err.to_string().contains("failed to read bundle file"));
    }

    #[test]
    fn yaml_value_kind_name_covers_every_variant() {
        assert_eq!(yaml_value_kind_name(&serde_yaml::Value::Null), "null");
        assert_eq!(
            yaml_value_kind_name(&serde_yaml::Value::Bool(true)),
            "a bool"
        );
        assert_eq!(
            yaml_value_kind_name(&serde_yaml::from_str("1").unwrap()),
            "a number"
        );
        assert_eq!(
            yaml_value_kind_name(&serde_yaml::Value::String("x".into())),
            "a string"
        );
        assert_eq!(
            yaml_value_kind_name(&serde_yaml::from_str("[1]").unwrap()),
            "a sequence"
        );
        assert_eq!(
            yaml_value_kind_name(&serde_yaml::from_str("a: 1").unwrap()),
            "a mapping"
        );
        let tagged = serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("custom"),
            value: serde_yaml::Value::String("x".into()),
        }));
        assert_eq!(yaml_value_kind_name(&tagged), "a tagged value");
    }

    #[test]
    fn parse_plugin_mcp_entries_missing_file_is_empty() {
        // #915: a missing MCP file means the plugin ships no MCP config — empty,
        // not an error (the caller no longer guards with exists()).
        let tmp = tempfile::tempdir().unwrap();
        let entries =
            parse_plugin_mcp_entries(&tmp.path().join("does-not-exist"), "my-plugin").unwrap();
        assert!(entries.is_empty());
    }

    // #915: a non-NotFound read error (EACCES) still errors, so it isn't
    // silently dropped now that the exists() guard is gone.
    #[cfg(unix)]
    #[test]
    fn parse_plugin_mcp_entries_unreadable_file_errors() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        let mcp_path = dir.join("LLM_PROVIDER_MCP_JSON");
        std::fs::write(&mcp_path, "{}").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = parse_plugin_mcp_entries(&mcp_path, "my-plugin");
        let readable_anyway = std::fs::read_dir(&dir).is_ok();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(result.unwrap_err().to_string().contains("failed to read"));
    }

    #[test]
    fn parse_plugin_mcp_entries_invalid_yaml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mcp_path = tmp.path().join("LLM_PROVIDER_MCP_JSON");
        std::fs::write(&mcp_path, ": invalid yaml :::").unwrap();
        let err = parse_plugin_mcp_entries(&mcp_path, "my-plugin").unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn parse_plugin_hooks_missing_file_is_none() {
        // #915: a missing hooks manifest of this name yields Ok(None) — the
        // caller tries the next candidate name — not an error.
        let tmp = tempfile::tempdir().unwrap();
        let hooks =
            parse_plugin_hooks(&tmp.path().join("hooks.json"), tmp.path(), "my-plugin").unwrap();
        assert!(hooks.is_none());
    }

    #[test]
    fn materialize_rules_dest_dir_creation_failure_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create "rules" as a plain FILE so create_dir_all for the rule's
        // parent directory fails (can't mkdir where a file already exists).
        std::fs::write(tmp.path().join("rules"), b"blocking file").unwrap();
        let mut manifest = MergedManifest::default();
        manifest.rules.push(RuleFile {
            bundle: "test".into(),
            rel: PathBuf::from("rules/security.md"),
            frontmatter: None,
            body: "content".into(),
            raw: "content".into(),
        });
        assert!(OpencodeAdapter.materialize(&manifest, tmp.path()).is_err());
    }

    #[test]
    fn materialize_plugin_resolution_failure_propagates() {
        let out = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            plugins: vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: "missing-marketplace".into(),
                plugin: "some-plugin".into(),
                collection: String::new(),
                install_path: None,
                git_commit_sha: None,
            }],
            marketplaces: vec![],
            ..Default::default()
        };
        let err = OpencodeAdapter
            .materialize(&manifest, out.path())
            .unwrap_err();
        assert!(err.to_string().contains("missing-marketplace"));
    }

    #[test]
    fn context_mode_is_not_registered_as_an_opencode_plugin() {
        let out = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            plugins: vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: "test".into(),
                plugin: "context-mode".into(),
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
        let raw = std::fs::read_to_string(out.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Its 1.x plugin cannot load on opencode 2.0 (#2840).
        assert!(doc.get("plugin").is_none(), "{doc}");
    }

    #[test]
    fn materialize_lsp_init_options_conversion_failure_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.lsp.push(llmenv_config::LspServer {
            name: "broken-ls".into(),
            command: "broken-ls".into(),
            // A YAML complex mapping key (sequence-as-key) has no JSON
            // representation ("key must be a string"), so serde_json::to_value
            // fails — unlike NaN or an integer key, both of which serde_json
            // silently coerces (to `null` and a stringified key, respectively).
            init_options: Some(serde_yaml::from_str("? [a, b]\n: foo\n").unwrap()),
            ..Default::default()
        });
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(err.to_string().contains("broken-ls"));
    }

    #[test]
    fn materialize_mcp_omitted_when_native_overlay_yields_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        // No manifest.mcps entries, but native_mcp.opencode is present (empty
        // mapping) — the `if` guard trips on the native_mcp key alone, and the
        // resulting merged object is still empty, so `mcp` must be omitted.
        manifest.capabilities.native_mcp.insert(
            "opencode".into(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(doc.get("mcp").is_none());
    }

    #[test]
    fn materialize_bundle_unreadable_agent_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.files.insert(
            PathBuf::from("agents/missing.md"),
            PathBuf::from("/does/not/exist.md"),
        );
        let err = OpencodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        assert!(err.to_string().contains("failed to read bundle file"));
    }

    #[test]
    fn materialize_native_opencode_overlay_merges_extra_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        let frag: serde_yaml::Value = serde_yaml::from_str("theme: dark\n").unwrap();
        manifest.native.insert("opencode".into(), frag);
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["theme"], serde_json::json!("dark"));
    }

    // #1331: opencode gets the same `manifest.mcps` Claude Code does, so the
    // same `index_repository` clobber is reachable here. The hook names only the tools that
    // need it, so no other tool call pays for it.
    #[test]
    fn index_repository_guard_hook_tracks_the_codebase_memory_mcp() {
        let with =
            hook_bridge::hook_table(&[], &["mcp__x__index_repository", "Bash"], &[]).unwrap();
        let without = hook_bridge::hook_table(&[], &[], &[]).unwrap().to_string();
        let entry = with["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| {
                h["command"] == hook_command("hook-run pre_tool_use --engine opencode").unwrap()
            })
            .unwrap();
        assert_eq!(entry["matcher"], "mcp__x__index_repository|Bash");
        assert!(
            !without.contains("hook-run pre_tool_use"),
            "no MCP is wired, so every tool call would pay for a hook that \
             can never fire"
        );
    }

    #[test]
    fn a_hook_with_no_command_is_left_out() {
        let hooks = vec![crate::config::Hook {
            event: "PostToolUse".into(),
            matcher: None,
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::McpTool,
                command: None,
                tool: Some("x".into()),
            },
            bundle_origin: None,
        }];
        let table = hook_bridge::hook_table(&hooks, &[], &[]).unwrap();
        let events: Vec<_> = table["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["event"].as_str().unwrap().to_string())
            .collect();
        assert!(!events.contains(&"PostToolUse".to_string()), "{events:?}");
    }

    #[test]
    fn a_hook_keeps_its_matcher_and_falls_back_to_the_raw_command() {
        let hooks = vec![crate::config::Hook {
            event: "PreToolUse".into(),
            matcher: Some("Bash".into()),
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::Command,
                command: Some("echo hello".into()),
                tool: None,
            },
            bundle_origin: Some(PathBuf::from("/bundles/foo")),
        }];
        // "echo hello" has no '/' token, so resolve_bundle_relative_paths
        // returns None and the raw command is used.
        let table = hook_bridge::hook_table(&hooks, &[], &["codebase-memory-mcp"]).unwrap();
        assert_eq!(table["hooks"][0]["command"], "echo hello");
        assert_eq!(table["hooks"][0]["matcher"], "Bash");
        assert_eq!(
            table["mcpServers"]["codebase-memory-mcp"],
            "codebase-memory-mcp"
        );
        assert_eq!(table["tools"]["shell"]["name"], "Bash");
    }

    // -- split_frontmatter --

    #[test]
    fn split_frontmatter_simple() {
        let (fm, body) = split_frontmatter("---\ndescription: hello\n---\nbody text\n");
        assert!(fm.is_some(), "frontmatter must be parsed");
        assert_eq!(body.trim(), "body text");
    }

    #[test]
    fn split_frontmatter_absent() {
        let (fm, body) = split_frontmatter("just body text without frontmatter");
        assert!(fm.is_none(), "no frontmatter expected");
        assert_eq!(body.trim(), "just body text without frontmatter");
    }

    #[test]
    fn split_frontmatter_empty_source() {
        let (fm, body) = split_frontmatter("");
        assert!(fm.is_none());
        assert!(body.is_empty());
    }

    #[test]
    fn split_frontmatter_only_delimiter() {
        // A single "---" with no closer or body.
        let (fm, body) = split_frontmatter("---");
        assert!(fm.is_none(), "no closing ---, no frontmatter");
        assert_eq!(body.trim(), "---");
    }

    #[test]
    fn split_frontmatter_empty_frontmatter() {
        let (fm, body) = split_frontmatter("---\n---\nbody\n");
        assert!(fm.is_some(), "empty frontmatter is valid YAML");
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn split_frontmatter_invalid_yaml() {
        // Malformed YAML should warn (via tracing) and return None.
        let (fm, body) = split_frontmatter("---\n: invalid yaml :::\n---\nbody\n");
        assert!(fm.is_none(), "invalid YAML returns None frontmatter");
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn split_frontmatter_preserves_body_trailing_whitespace() {
        // The raw body after the closing --- delimiter is returned as-is
        // (not trimmed) so markdown line-endings are preserved.
        let (_, body) = split_frontmatter("---\nkey: val\n---\nstrong text\n\nmore\n");
        assert_eq!(body, "strong text\n\nmore\n");
    }

    // -- property-based tests: permission translation (opencode-unique) --
    //
    // opencode's per-tool pattern→action permission map went through three
    // format-fix commits (bare-tool action-string vs `{"*": action}` wrapper,
    // deny-wins last-write shadowing). These properties assert the render
    // invariants hold across arbitrary rule sets, exercised through the public
    // `materialize` API rather than the private helper fns.
    use proptest::prelude::*;

    use crate::adapter::skills::{arb_hook, arb_permission_rule};

    /// Materialize a manifest carrying only `caps` and return the emitted
    /// `permission` value (or `Null` when the key is absent).
    fn permission_value(caps: crate::config::Capabilities) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            capabilities: caps,
            ..Default::default()
        };
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        perm_view(&doc)
    }

    fn is_action(s: &str) -> bool {
        matches!(s, "allow" | "ask" | "deny")
    }

    proptest! {
        /// For any allow/ask/deny rule set the emitted `permission` value is
        /// well-formed: absent, or an object whose every entry is an action
        /// string or a non-empty pattern→action map with action-string values.
        /// Never panics, never errors.
        #[test]
        fn permission_map_is_always_well_formed(
            allow in proptest::collection::vec(arb_permission_rule(), 0..5),
            ask in proptest::collection::vec(arb_permission_rule(), 0..5),
            deny in proptest::collection::vec(arb_permission_rule(), 0..5),
        ) {
            let mut caps = crate::config::Capabilities::default();
            caps.permissions.allow = allow;
            caps.permissions.ask = ask;
            caps.permissions.deny = deny;
            match permission_value(caps) {
                serde_json::Value::Null => {}
                serde_json::Value::Object(tools) => {
                    for (_tool, val) in tools {
                        match val {
                            serde_json::Value::String(s) => {
                                prop_assert!(is_action(&s), "bare action must be allow/ask/deny: {s}");
                            }
                            serde_json::Value::Object(patterns) => {
                                prop_assert!(!patterns.is_empty(), "pattern map must not be empty");
                                for (_pat, act) in patterns {
                                    let s = act.as_str().unwrap_or_default();
                                    prop_assert!(is_action(s), "pattern action must be allow/ask/deny: {s}");
                                }
                            }
                            other => prop_assert!(false, "unexpected permission value: {other}"),
                        }
                    }
                }
                other => prop_assert!(false, "permission must be object or absent: {other}"),
            }
        }

        /// A single bare rule (no pattern, no paths) for an otherwise-unused
        /// tool renders as a plain action string, not a `{"*": action}` object.
        #[test]
        fn bare_rule_renders_action_string(tool in arb_mapped_neutral_tool()) {
            let mut caps = crate::config::Capabilities::default();
            caps.permissions.allow.push(crate::config::PermissionRule {
                tool: tool.clone(),
                pattern: None,
                paths: vec![],
            });
            prop_assert_eq!(
                &permission_value(caps)[opencode_tool_name(&tool).unwrap()],
                &serde_json::json!("allow")
            );
        }

        /// When the same tool+pattern is both allowed and denied, deny wins
        /// (deny is inserted last — documented last-write-wins semantics).
        #[test]
        fn deny_overrides_allow_for_same_tool_pattern(
            tool in arb_mapped_neutral_tool(),
            pattern in "[a-z]{2,8}",
        ) {
            let rule = |t: &str, p: &str| crate::config::PermissionRule {
                tool: t.to_string(),
                pattern: Some(p.to_string()),
                paths: vec![],
            };
            let mut caps = crate::config::Capabilities::default();
            caps.permissions.allow.push(rule(&tool, &pattern));
            caps.permissions.deny.push(rule(&tool, &pattern));
            prop_assert_eq!(
                &permission_value(caps)[opencode_tool_name(&tool).unwrap()][&pattern],
                &serde_json::json!("deny")
            );
        }

        /// Generalizes the #877 regression: for a given tool+pattern, the
        /// highest-precedence tier present (deny > ask > allow) wins the
        /// final action, regardless of whether that tier's rule came from
        /// the structured `permissions:` block or the `native_permissions`
        /// override, and regardless of which other tiers/sources are also
        /// present.
        #[test]
        fn highest_tier_wins_regardless_of_source(
            tool in arb_mapped_neutral_tool(),
            pattern in "[a-z]{2,8}",
            allow_present in any::<bool>(),
            allow_native in any::<bool>(),
            ask_present in any::<bool>(),
            ask_native in any::<bool>(),
            deny_present in any::<bool>(),
            deny_native in any::<bool>(),
        ) {
            prop_assume!(allow_present || ask_present || deny_present);

            let rule = |t: &str, p: &str| crate::config::PermissionRule {
                tool: t.to_string(),
                pattern: Some(p.to_string()),
                paths: vec![],
            };
            let mut caps = crate::config::Capabilities::default();
            let mut native = crate::config::NativePermissionRules::default();
            for (present, is_native, structured_tier, native_tier) in [
                (allow_present, allow_native, &mut caps.permissions.allow, &mut native.allow),
                (ask_present, ask_native, &mut caps.permissions.ask, &mut native.ask),
                (deny_present, deny_native, &mut caps.permissions.deny, &mut native.deny),
            ] {
                if !present {
                    continue;
                }
                if is_native {
                    native_tier.push(format!("{tool}({pattern})"));
                } else {
                    structured_tier.push(rule(&tool, &pattern));
                }
            }
            caps.native_permissions.insert("opencode".into(), native);

            let expected = if deny_present {
                "deny"
            } else if ask_present {
                "ask"
            } else {
                "allow"
            };
            prop_assert_eq!(
                &permission_value(caps)[opencode_tool_name(&tool).unwrap()][&pattern],
                &serde_json::json!(expected)
            );
        }

        /// Rendering is deterministic: identical rule sets produce identical
        /// permission output.
        #[test]
        fn permission_render_is_deterministic(
            allow in proptest::collection::vec(arb_permission_rule(), 0..5),
            deny in proptest::collection::vec(arb_permission_rule(), 0..5),
        ) {
            let build = || {
                let mut caps = crate::config::Capabilities::default();
                caps.permissions.allow = allow.clone();
                caps.permissions.deny = deny.clone();
                permission_value(caps)
            };
            prop_assert_eq!(build(), build());
        }
    }

    // -- property-based tests: remaining serialization/parsing surfaces --
    //
    // Closes gaps identified in pre-pr-review of #876: LSP rendering,
    // split_frontmatter parser robustness, translate_*_md idempotence,
    // the generated hook shim's embedded JSON, and native permission-rule
    // parsing (rule_to_patterns / parse_native_rule, exercised indirectly
    // through `materialize` since those helpers are private to it).

    /// Strategy for a native permission rule string that never contains `(`
    /// or `*`, so it always takes the bare-tool wildcard path.
    /// `parse_native_rule` lowercases verbatim — no name mapping, #1326's
    /// security-audit finding — so any non-empty, paren-free string works.
    fn arb_native_rule_tool() -> impl Strategy<Value = String> {
        "[A-Za-z]{1,8}".prop_map(String::from)
    }

    /// Strategy for a *neutral* (structured `permissions:`) tool name that's
    /// guaranteed to map successfully via `opencode_tool_name`, which is now
    /// a partial function (#1326) covering only confirmed-valid Claude Code
    /// tool names. The tool identity is incidental to what proptests using
    /// this check (precedence/dedup logic on structured rules), but it must
    /// still map successfully for the JSON index in the assertion to be
    /// meaningful.
    ///
    /// Excludes `Write` deliberately: `highest_tier_wins_regardless_of_source`
    /// pushes the same generated `tool` string through *both* the structured
    /// path (mapped via `opencode_tool_name`, `Write` -> `edit`) and the
    /// native path (`parse_native_rule`, lowercased verbatim, `Write` ->
    /// `write`) for the same test case — those two paths must agree on the
    /// final key for the "regardless of source" premise to hold, and `Write`
    /// is precisely the one name where native and structured intentionally
    /// diverge (#1326's security-audit finding).
    fn arb_mapped_neutral_tool() -> impl Strategy<Value = String> {
        prop::sample::select(&["Bash", "Read", "Edit", "Glob", "Grep"][..]).prop_map(String::from)
    }

    proptest! {
        /// `split_frontmatter` must never panic, regardless of input —
        /// including inputs with multi-byte UTF-8 characters straddling the
        /// byte offsets the parser slices at.
        #[test]
        fn split_frontmatter_never_panics(input in ".*") {
            let _ = split_frontmatter(&input);
        }

        /// When the (whitespace-trimmed) input doesn't open with `---`, the
        /// body returned is the exact original source — no frontmatter parsed.
        #[test]
        fn split_frontmatter_body_equals_source_without_delimiter(rest in ".{0,40}") {
            // Prefix with a non-dash, non-whitespace char so trim_start() can
            // never expose a leading "---" hiding after whitespace.
            let source = format!("x{rest}");
            let (fm, body) = split_frontmatter(&source);
            prop_assert!(fm.is_none());
            prop_assert_eq!(body, source.as_str());
        }

        /// LSP rendering: `extensions`/`env` are present iff the source
        /// server config has filetypes/env entries; `command` always starts
        /// with the configured command and has one entry per arg.
        #[test]
        fn lsp_optional_fields_omitted_when_absent(
            name in "[a-z]{1,8}",
            command in "[a-z]{1,8}",
            args in proptest::collection::vec("[a-z]{1,4}", 0..3),
            filetypes in proptest::collection::vec("[a-z]{1,4}", 0..3),
            env_value in proptest::option::of("[a-z]{1,4}"),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let mut manifest = MergedManifest::default();
            let mut env = std::collections::BTreeMap::new();
            if let Some(v) = &env_value {
                env.insert("KEY".to_string(), v.clone());
            }
            manifest.capabilities.lsp.push(llmenv_config::LspServer {
                name: name.clone(),
                command: command.clone(),
                args: args.clone(),
                filetypes: filetypes.clone(),
                env,
                ..Default::default()
            });
            OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
            let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let srv = &doc["lsp"][&name];
            let cmd = srv["command"].as_array().unwrap();
            prop_assert_eq!(cmd.len(), 1 + args.len());
            prop_assert_eq!(cmd[0].as_str().unwrap(), command.as_str());
            prop_assert_eq!(srv.get("extensions").is_some(), !filetypes.is_empty());
            prop_assert_eq!(srv.get("env").is_some(), env_value.is_some());
            prop_assert!(srv.get("initialization").is_none());
        }

        /// `translate_agent_md` always inserts `mode: subagent`, regardless
        /// of whether the source had a `model` field.
        #[test]
        fn translate_agent_md_always_sets_mode_subagent(has_model in any::<bool>()) {
            let source = if has_model {
                "---\ndescription: d\nmodel: sonnet\n---\nbody\n"
            } else {
                "---\ndescription: d\n---\nbody\n"
            };
            let out = translate_agent_md(source).unwrap();
            let (fm, _) = split_frontmatter(&out);
            let fm = fm.unwrap();
            prop_assert_eq!(
                fm.as_mapping().unwrap().get(serde_yaml::Value::String("mode".into())),
                Some(&serde_yaml::Value::String("subagent".into()))
            );
        }

        /// Applying `translate_agent_md` / `translate_command_md` twice is a
        /// no-op the second time — dropped fields don't get "more dropped".
        #[test]
        fn translate_md_transforms_are_idempotent(has_extra in any::<bool>()) {
            let agent_source = if has_extra {
                "---\ndescription: d\nmodel: sonnet\n---\nbody\n"
            } else {
                "---\ndescription: d\n---\nbody\n"
            };
            let agent_once = translate_agent_md(agent_source).unwrap();
            let agent_twice = translate_agent_md(&agent_once).unwrap();
            prop_assert_eq!(agent_once, agent_twice);

            let command_source = if has_extra {
                "---\ndescription: d\nallowed_tools: [Bash]\n---\nbody\n"
            } else {
                "no frontmatter here\n"
            };
            let command_once = translate_command_md(command_source).unwrap();
            let command_twice = translate_command_md(&command_once).unwrap();
            prop_assert_eq!(command_once, command_twice);
        }

        /// Every hook with a command reaches the table with its event, matcher, and command,
        /// after the user hooks and before nothing else, and the table is deterministic.
        #[test]
        fn every_command_hook_reaches_the_table_in_order(
            hooks in proptest::collection::vec(arb_hook(), 0..5)
        ) {
            let table = hook_bridge::hook_table(&hooks, &[], &[]).unwrap();
            prop_assert_eq!(&table, &hook_bridge::hook_table(&hooks, &[], &[]).unwrap());
            let entries = table["hooks"].as_array().unwrap();
            let with_command: Vec<_> = hooks.iter().filter(|h| h.handler.command.is_some()).collect();
            for (hook, entry) in with_command.iter().zip(entries) {
                prop_assert_eq!(entry["event"].as_str(), Some(hook.event.as_str()));
                prop_assert_eq!(entry["matcher"].as_str(), hook.matcher.as_deref());
            }
            // The three built-in hooks follow the user hooks.
            prop_assert_eq!(entries.len(), with_command.len() + 3);
        }

        /// A bare native rule (no parens) wildcard-matches and lowercases the
        /// tool name — exercised through `native_permissions.opencode` since
        /// `parse_native_rule` is private to `materialize`.
        #[test]
        fn native_rule_bare_tool_wildcards_and_lowercases(tool in arb_native_rule_tool()) {
            let mut caps = crate::config::Capabilities::default();
            caps.native_permissions.insert(
                "opencode".to_string(),
                crate::config::NativePermissionRules {
                    allow: vec![tool.clone()],
                    ..Default::default()
                },
            );
            let val = permission_value(caps);
            prop_assert_eq!(&val[tool.to_ascii_lowercase()], &serde_json::json!("allow"));
        }

        /// A native rule with a parenthesized pattern extracts that pattern
        /// verbatim and maps only the tool portion (#1326).
        #[test]
        fn native_rule_with_parens_extracts_pattern(
            tool in arb_native_rule_tool(),
            pattern in "[a-z]{1,6}",
        ) {
            let mut caps = crate::config::Capabilities::default();
            caps.native_permissions.insert(
                "opencode".to_string(),
                crate::config::NativePermissionRules {
                    allow: vec![format!("{tool}({pattern})")],
                    ..Default::default()
                },
            );
            let val = permission_value(caps);
            prop_assert_eq!(
                &val[tool.to_ascii_lowercase()][&pattern],
                &serde_json::json!("allow")
            );
        }

        /// `materialize` never panics on an arbitrary native permission rule
        /// string, no matter how malformed — it always returns `Ok` or `Err`.
        #[test]
        fn native_rule_arbitrary_string_never_panics(rule in ".{0,20}") {
            let mut caps = crate::config::Capabilities::default();
            caps.native_permissions.insert(
                "opencode".to_string(),
                crate::config::NativePermissionRules {
                    allow: vec![rule],
                    ..Default::default()
                },
            );
            let manifest = MergedManifest {
                capabilities: caps,
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let _ = OpencodeAdapter.materialize(&manifest, tmp.path());
        }

        /// A rule string with only one of the two parens present (never
        /// both) is always rejected, regardless of the tool/pattern content
        /// around it.
        #[test]
        fn native_rule_single_paren_is_always_error(
            tool in "[A-Za-z]{1,8}",
            pattern in "[a-z]{1,8}",
            keep_open in any::<bool>(),
        ) {
            let rule = if keep_open {
                format!("{tool}({pattern}")
            } else {
                format!("{tool}{pattern})")
            };
            let mut caps = crate::config::Capabilities::default();
            caps.native_permissions.insert(
                "opencode".to_string(),
                crate::config::NativePermissionRules {
                    allow: vec![rule],
                    ..Default::default()
                },
            );
            let manifest = MergedManifest {
                capabilities: caps,
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            prop_assert!(OpencodeAdapter.materialize(&manifest, tmp.path()).is_err());
        }

        /// `parse_plugin_hooks` never panics on arbitrary file content, valid
        /// JSON or not.
        #[test]
        fn parse_plugin_hooks_never_panics_on_arbitrary_content(content in ".{0,200}") {
            let tmp = tempfile::tempdir().unwrap();
            let hooks_path = tmp.path().join("hooks.json");
            std::fs::write(&hooks_path, &content).unwrap();
            let _ = parse_plugin_hooks(&hooks_path, tmp.path(), "test-plugin");
        }

        /// #1017: `parse_plugin_mcp_entries` deserializes a plugin-provided
        /// `LLM_PROVIDER_MCP_JSON` file — genuinely untrusted input, unlike
        /// the adapter's own `McpEntry` construction path — and must never
        /// panic regardless of content.
        #[test]
        fn parse_plugin_mcp_entries_never_panics_on_arbitrary_content(content in ".{0,200}") {
            let tmp = tempfile::tempdir().unwrap();
            let mcp_path = tmp.path().join("LLM_PROVIDER_MCP_JSON");
            std::fs::write(&mcp_path, &content).unwrap();
            let _ = parse_plugin_mcp_entries(&mcp_path, "test-plugin");
        }
    }

    #[test]
    fn config_schema_returns_a_schema_describing_the_authored_keys() {
        let schema = OpencodeAdapter.config_schema().unwrap();
        assert_eq!(
            schema["$schema"],
            serde_json::json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(schema["additionalProperties"], serde_json::json!(true));
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("mcp"), "schema must describe 'mcp'");
        assert!(props.contains_key("lsp"), "schema must describe 'lsp'");
        assert!(
            props.contains_key("permissions"),
            "schema must describe 'permissions'"
        );
    }

    #[test]
    fn minimal_mcp_entries_satisfy_the_generated_schemas_required_fields() {
        let schema = OpencodeAdapter.config_schema().unwrap();
        let variants = schema["$defs"]["McpEntry"]["oneOf"].as_array().unwrap();

        let local = serde_json::to_value(McpEntry::Local {
            command: vec!["node".into()],
            environment: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout: None,
        })
        .unwrap();
        let remote = serde_json::to_value(McpEntry::Remote {
            url: "https://example.com".into(),
            headers: BTreeMap::new(),
            timeout: None,
        })
        .unwrap();

        for (entry, type_tag) in [(&local, "local"), (&remote, "remote")] {
            let variant_schema = variants
                .iter()
                .find(|v| v["properties"]["type"]["const"] == serde_json::json!(type_tag))
                .unwrap();
            for required_key in variant_schema["required"].as_array().unwrap() {
                let key = required_key.as_str().unwrap();
                assert!(
                    entry.get(key).is_some(),
                    "minimal '{type_tag}' entry {entry} is missing required key '{key}' \
                     the schema demands — schemars marks a field required unless it's \
                     Option or has #[serde(default)]"
                );
            }
        }
    }

    #[test]
    fn materialize_writes_schema_sidecar_alongside_opencode_json() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();

        let sidecar_path = tmp.path().join("opencode.schema.json");
        assert!(
            sidecar_path.exists(),
            "expected opencode.schema.json sidecar to be written"
        );
        let raw = std::fs::read_to_string(&sidecar_path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["type"], serde_json::json!("object"));
        assert!(owned.contains(&PathBuf::from("opencode.schema.json")));
    }

    // -- property-based tests: McpEntry roundtrip + MCP assembly + schema drift --
    //
    // Follow-up from pre-pr-review of #1010 (opencode schema sidecar wiring,
    // #1001): closes gaps flagged by property-test-gap-finder (#1011, #1012).

    fn arb_mcp_entry() -> impl Strategy<Value = McpEntry> {
        let local = (
            proptest::collection::vec("[a-z][a-z0-9_./-]{0,9}", 1..4),
            arb_string_map(),
            arb_string_map(),
            proptest::option::of(0u32..10_000),
        )
            .prop_map(|(command, environment, headers, timeout)| McpEntry::Local {
                command,
                environment,
                headers,
                timeout,
            });
        let remote = (
            "https?://[a-z0-9.]{3,20}",
            arb_string_map(),
            proptest::option::of(0u32..10_000),
        )
            .prop_map(|(url, headers, timeout)| McpEntry::Remote {
                url,
                headers,
                timeout,
            });
        prop_oneof![local, remote]
    }

    // -- property-based tests: remaining typed opencode.json structs roundtrip --
    //
    // Follow-up from pre-pr-review of #1013 (#1016): #1013 only covered
    // `McpEntry`. The same Serialize-only-but-should-roundtrip pattern exists
    // on the rest of the typed `opencode.json` output structs.

    /// Shared JSON-roundtrip assertion for the `#[cfg_attr(test, derive(...))]`
    /// structs below — six identical three-line bodies collapsed into one.
    fn assert_json_roundtrip<T>(value: T) -> Result<(), proptest::test_runner::TestCaseError>
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(&value).unwrap();
        let parsed: T = serde_json::from_value(json).unwrap();
        prop_assert_eq!(value, parsed);
        Ok(())
    }

    fn arb_permission_rule_json() -> impl Strategy<Value = permissions::Rule> {
        ("[a-z*]{1,8}", "[a-z* ]{1,8}", "allow|ask|deny").prop_map(|(action, resource, effect)| {
            permissions::Rule {
                action,
                resource,
                effect,
            }
        })
    }

    /// Arbitrary shallow JSON value for `LspServerEntry::init_options`, which
    /// passes through as opaque `serde_json::Value`. No `Value::Null` — that's
    /// a real serde quirk, not a coverage gap: `Option<Value>`'s `Deserialize`
    /// impl maps JSON `null` back to `None`, not `Some(Value::Null)`, so
    /// `Some(Null)` can never roundtrip regardless of this adapter's code.
    fn arb_init_options_value() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            proptest::bool::ANY.prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| serde_json::json!(n)),
            "[a-z]{0,8}".prop_map(serde_json::Value::String),
        ]
    }

    fn arb_lsp_server_entry() -> impl Strategy<Value = LspServerEntry> {
        (
            proptest::collection::vec("[a-z]{1,8}", 1..3),
            proptest::option::of(proptest::collection::vec("[a-z]{1,6}", 0..3)),
            proptest::option::of(arb_string_map()),
            proptest::option::of(arb_init_options_value()),
        )
            .prop_map(|(command, extensions, env, init_options)| LspServerEntry {
                command,
                extensions,
                env,
                init_options,
            })
    }

    fn arb_opencode_model_limit() -> impl Strategy<Value = OpencodeModelLimit> {
        (0u32..1_000_000, 0u32..1_000_000)
            .prop_map(|(context, output)| OpencodeModelLimit { context, output })
    }

    fn arb_opencode_model_cost() -> impl Strategy<Value = OpencodeModelCost> {
        (
            0.0..100.0f64,
            0.0..100.0f64,
            proptest::option::of(0.0..100.0f64),
            proptest::option::of(0.0..100.0f64),
        )
            .prop_map(
                |(input, output, cache_read, cache_write)| OpencodeModelCost {
                    input,
                    output,
                    cache_read,
                    cache_write,
                },
            )
    }

    fn arb_opencode_modalities() -> impl Strategy<Value = OpencodeModalities> {
        proptest::collection::vec("[a-z]{1,8}", 0..3).prop_map(|input| OpencodeModalities { input })
    }

    fn arb_opencode_model_entry() -> impl Strategy<Value = OpencodeModelEntry> {
        (
            proptest::option::of("[a-z ]{1,10}"),
            proptest::option::of(arb_opencode_model_limit()),
            proptest::option::of(arb_opencode_model_cost()),
            proptest::option::of(arb_opencode_modalities()),
        )
            .prop_map(|(name, limit, cost, modalities)| OpencodeModelEntry {
                name,
                limit,
                cost,
                modalities,
            })
    }

    fn arb_opencode_provider_options() -> impl Strategy<Value = OpencodeProviderOptions> {
        (
            proptest::option::of("https?://[a-z0-9.]{3,20}"),
            proptest::option::of("[a-z0-9]{1,16}"),
            proptest::option::of(arb_string_map()),
        )
            .prop_map(|(base_url, api_key, headers)| OpencodeProviderOptions {
                base_url,
                api_key,
                headers,
            })
    }

    fn arb_opencode_provider_entry() -> impl Strategy<Value = OpencodeProviderEntry> {
        (
            "[a-z@/-]{1,20}",
            proptest::option::of("[a-z ]{1,10}"),
            arb_opencode_provider_options(),
            proptest::option::of(proptest::collection::btree_map(
                "[a-z0-9_-]{1,10}",
                arb_opencode_model_entry(),
                0..3,
            )),
        )
            .prop_map(|(npm, name, options, models)| OpencodeProviderEntry {
                npm,
                name,
                options,
                models,
            })
    }

    fn arb_opencode_config() -> impl Strategy<Value = OpencodeConfig> {
        (
            proptest::option::of(proptest::collection::vec("[a-z][a-z0-9_-]{0,10}", 0..3)),
            proptest::option::of(proptest::collection::btree_map(
                "[a-z][a-z0-9_-]{0,8}",
                arb_lsp_server_entry(),
                0..3,
            )),
            "[a-z0-9./-]{1,20}",
            proptest::option::of(proptest::collection::vec("[a-z/.]{1,10}", 0..3)),
            proptest::option::of(proptest::collection::vec(arb_permission_rule_json(), 0..3)),
            proptest::option::of("[a-z][a-z0-9_/-]{0,15}"),
            proptest::option::of("[a-z][a-z0-9_/-]{0,15}"),
        )
            .prop_map(
                |(plugin, lsp, schema, instructions, permissions, model, small_model)| {
                    OpencodeConfig {
                        plugin,
                        mcp: None,
                        lsp,
                        schema,
                        instructions,
                        permissions,
                        provider: None,
                        model,
                        small_model,
                    }
                },
            )
    }

    proptest! {
        /// #1011: `McpEntry` must roundtrip losslessly through JSON — the
        /// type is only privately used inside `materialize`, so nothing else
        /// exercises `Deserialize` today.
        #[test]
        fn mcp_entry_roundtrips_through_json(entry in arb_mcp_entry()) {
            assert_json_roundtrip(entry)?;
        }

        #[test]
        fn permission_rule_roundtrips_through_json(rule in arb_permission_rule_json()) {
            assert_json_roundtrip(rule)?;
        }

        #[test]
        fn lsp_server_entry_roundtrips_through_json(entry in arb_lsp_server_entry()) {
            assert_json_roundtrip(entry)?;
        }

        #[test]
        fn opencode_model_entry_roundtrips_through_json(entry in arb_opencode_model_entry()) {
            assert_json_roundtrip(entry)?;
        }

        #[test]
        fn opencode_provider_options_roundtrips_through_json(
            options in arb_opencode_provider_options()
        ) {
            assert_json_roundtrip(options)?;
        }

        /// #1016: `OpencodeProviderEntry` composes `OpencodeProviderOptions`
        /// and `OpencodeModelEntry` (which itself composes
        /// `OpencodeModelLimit`/`OpencodeModelCost`/`OpencodeModalities`), so
        /// this roundtrip transitively exercises all of them.
        #[test]
        fn opencode_provider_entry_roundtrips_through_json(entry in arb_opencode_provider_entry()) {
            assert_json_roundtrip(entry)?;
        }

        /// #1016: `OpencodeConfig` is the root type composing the typed
        /// fields (`lsp`, `permission`) directly — `mcp`/`provider` stay
        /// `serde_json::Value` here since that's their real field type (they
        /// go through a native-overlay merge after construction), covered
        /// separately by `mcp_entry_roundtrips_through_json` and
        /// `opencode_provider_entry_roundtrips_through_json`.
        #[test]
        fn opencode_config_roundtrips_through_json(config in arb_opencode_config()) {
            assert_json_roundtrip(config)?;
        }
    }

    proptest! {
        /// #1012 task 2: for any arbitrary combination of stdio/remote MCP
        /// servers, `materialize`'s MCP-assembly loop emits exactly one
        /// `mcp.<name>` entry per server, tagged with the type matching its
        /// `ResolvedKind` — and never panics regardless of headers/timeout.
        #[test]
        fn mcp_assembly_produces_one_entry_per_server_with_correct_type(
            mcps in arb_distinct_resolved_mcps()
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let manifest = MergedManifest {
                mcps: mcps.clone(),
                ..Default::default()
            };
            OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
            let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();

            if mcps.is_empty() {
                prop_assert!(doc.get("mcp").is_none());
                return Ok(());
            }
            let mcp_obj = doc["mcp"].as_object().unwrap();
            prop_assert_eq!(mcp_obj.len(), mcps.len());
            for m in &mcps {
                let expected_type = match &m.kind {
                    ResolvedKind::Stdio { .. } => "local",
                    ResolvedKind::Remote { .. } => "remote",
                };
                prop_assert_eq!(mcp_obj[&m.name]["type"].as_str().unwrap(), expected_type);
            }
        }

        /// #1012 task 3: every top-level key `materialize` actually writes
        /// into `opencode.json` must have a matching property in the
        /// generated schema — otherwise `OpencodeConfig` and its schema have
        /// silently drifted apart. No JSON Schema validator dependency
        /// needed (none is a workspace dep yet, see #1012): this is a
        /// structural key-presence check, not full schema validation.
        ///
        /// Covers `$schema`, `mcp`, `permission`, `model`, `small_model` —
        /// the fields cheap to vary through `MergedManifest`. `plugin`,
        /// `lsp`, `instructions`, `provider` aren't exercised here; a
        /// follow-up can extend the strategy if those drift too.
        #[test]
        fn materialized_top_level_keys_are_declared_in_schema(
            mcps in arb_distinct_resolved_mcps(),
            allow in proptest::collection::vec(arb_permission_rule(), 0..3),
            large_model in proptest::option::of(("[a-z]{2,6}", "[a-z]{2,6}")),
            small_model in proptest::option::of(("[a-z]{2,6}", "[a-z]{2,6}")),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let mut caps = crate::config::Capabilities::default();
            caps.permissions.allow = allow;
            if let Some((provider, model)) = large_model {
                caps.default_models
                    .insert("large".into(), crate::config::ModelRef { provider, model });
            }
            if let Some((provider, model)) = small_model {
                caps.default_models
                    .insert("small".into(), crate::config::ModelRef { provider, model });
            }
            let manifest = MergedManifest {
                mcps,
                capabilities: caps,
                ..Default::default()
            };
            OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
            let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let schema = OpencodeAdapter.config_schema().unwrap();
            let props = schema["properties"].as_object().unwrap();

            for key in doc.as_object().unwrap().keys() {
                prop_assert!(
                    props.contains_key(key),
                    "materialized key '{key}' has no matching schema property — \
                     OpencodeConfig struct and its schema have drifted"
                );
            }
        }
    }

    // -- property-based tests: plugin-supplied config parsing (#1282) --
    //
    // The no-panic property over arbitrary content already exists above
    // (`parse_plugin_hooks_never_panics_on_arbitrary_content` /
    // `parse_plugin_mcp_entries_never_panics_on_arbitrary_content`, #1017).
    // What's missing is a round-trip property for *valid* manifests — added
    // below, reusing the shared `arb_yaml_value` generator (`skills.rs`)
    // rather than hand-rolling a new arbitrary-JSON generator for this file.

    use crate::adapter::skills::arb_yaml_value;

    /// A top-level YAML mapping of `name -> arbitrary value`, matching the
    /// shape `parse_plugin_mcp_entries` actually expects (a mapping of MCP
    /// server name to its config).
    fn arb_mcp_entries_mapping() -> impl Strategy<Value = serde_yaml::Mapping> {
        proptest::collection::vec(("[a-z][a-z0-9_-]{0,10}", arb_yaml_value(2)), 0..4).prop_map(
            |pairs| {
                let mut m = serde_yaml::Mapping::new();
                for (k, v) in pairs {
                    m.insert(serde_yaml::Value::String(k), v);
                }
                m
            },
        )
    }

    proptest! {
        /// A valid top-level mapping round-trips: every key survives, and
        /// each value converts to the same JSON shape production already
        /// asserts (`serde_json::to_value` over the YAML mapping).
        #[test]
        fn parse_plugin_mcp_entries_roundtrips_valid_mapping(mapping in arb_mcp_entries_mapping()) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("mcp.yaml");
            let yaml_value = serde_yaml::Value::Mapping(mapping.clone());
            std::fs::write(&path, serde_yaml::to_string(&yaml_value).unwrap()).unwrap();
            let parsed = parse_plugin_mcp_entries(&path, "test-plugin").unwrap();
            let expected = serde_json::to_value(&mapping)
                .unwrap()
                .as_object()
                .unwrap()
                .clone();
            prop_assert_eq!(parsed, expected);
        }

        /// A valid manifest with one event/group and 1..4 command hooks
        /// round-trips: every command hook is emitted in order, with its
        /// matcher preserved and `${CLAUDE_PLUGIN_ROOT}` resolved to the
        /// payload path — including when the placeholder appears embedded
        /// mid-string, not just as the whole command.
        #[test]
        fn parse_plugin_hooks_roundtrips_valid_manifest(
            event in "[A-Za-z]{3,12}",
            matcher in proptest::option::of("[a-z]{1,8}"),
            commands in proptest::collection::vec(
                prop_oneof![
                    llmenv_util::testkit::arb_hook_command_str(),
                    llmenv_util::testkit::arb_hook_command_str()
                        .prop_map(|s| format!("{s}${{CLAUDE_PLUGIN_ROOT}}{s}")),
                ],
                1..4,
            ),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let hooks_path = tmp.path().join("hooks.json");
            let payload_path = tmp.path().join("plugin-root");
            let manifest = serde_json::json!({
                "hooks": {
                    event.clone(): [
                        {
                            "matcher": matcher,
                            "hooks": commands
                                .iter()
                                .map(|c| serde_json::json!({"type": "command", "command": c}))
                                .collect::<Vec<_>>(),
                        }
                    ]
                }
            });
            std::fs::write(&hooks_path, serde_json::to_string(&manifest).unwrap()).unwrap();

            let hooks = parse_plugin_hooks(&hooks_path, &payload_path, "test-plugin")
                .unwrap()
                .unwrap();
            prop_assert_eq!(hooks.len(), commands.len());
            let payload_str = payload_path.to_string_lossy();
            for (hook, command) in hooks.iter().zip(commands.iter()) {
                prop_assert_eq!(&hook.event, &event);
                prop_assert_eq!(&hook.matcher, &matcher);
                let expected = command.replace("${CLAUDE_PLUGIN_ROOT}", &payload_str);
                prop_assert_eq!(hook.handler.command.as_deref(), Some(expected.as_str()));
            }
        }
    }
}
