//! The steps of `CrushAdapter::materialize` (#4055).
//!
//! One function holds one output file or one section of `crush.json`. A function keeps its `?`
//! operators few, because each one counts as a branch of the function.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::render::{self, CRUSH_MODELED_KEYS, render_default_models, render_lsp};
use super::{
    CRUSH_JSON_FILE, CRUSH_MD_FILE, SUPPORTED_HOOK_EVENTS, UNSUPPORTED_PLUGIN_DIRS, mcp,
    plugin_is_compatible,
};
use crate::adapter::hook_program::hook_command;
use crate::adapter::output_styles::ProjectedPluginSkill;
use crate::merge::MergedManifest;

type Doc = serde_json::Map<String, serde_json::Value>;

/// Write `crush.json` and the files that it points to. Returns the paths that the adapter owns.
pub(super) fn materialize(manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
    crate::adapter::skills::create_dir_owner_only(out)?;
    // The filter warns about each skipped hook, and those warnings come before any skill is
    // written.
    let hooks = compatible_hooks(manifest);
    let (owned, has_skills) = write_skills(manifest, out)?;
    // The gate that the Claude Code adapter runs too: frontmatter and the hardcoded-path scan.
    crate::adapter::skills::validate_skills(out)?;
    let doc = build_doc(manifest, out, has_skills, &hooks)?;
    let json_bytes = serde_json::to_vec_pretty(&doc)?;
    crate::paths::write_owner_only(&out.join(CRUSH_JSON_FILE), &json_bytes)?;
    Ok(owned)
}

/// Write the skills, the generated skills, and `CRUSH.md`. Returns the owned paths, and whether
/// any skill exists, which decides if `options.skills_paths` is set.
fn write_skills(manifest: &MergedManifest, out: &Path) -> anyhow::Result<(Vec<PathBuf>, bool)> {
    let skill_paths =
        crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;
    let mut owned = vec![PathBuf::from(CRUSH_JSON_FILE)];
    owned.extend(skill_paths.iter().cloned());
    owned.extend(write_instructions(manifest, out)?);
    let (plugin_skill_paths, projected) = project_plugins(manifest, out)?;
    owned.extend(plugin_skill_paths.iter().cloned());
    // Counted toward `skills_paths`, so Crush finds the built-in skill when it is the only one.
    let llmenv_skill_paths = crate::adapter::llmenv_skill::materialize_llmenv_skill(out, manifest)?;
    owned.extend(llmenv_skill_paths.iter().cloned());
    owned.extend(write_output_styles(manifest, out, &projected)?);
    // Plugin skills count too, or a set of plugin-only skills would omit the key (P1-2).
    let has_skills = [&skill_paths, &plugin_skill_paths, &llmenv_skill_paths]
        .iter()
        .any(|paths| !paths.is_empty());
    Ok((owned, has_skills))
}

/// Crush loads `CRUSH.md` from the config directory, so the merged instructions reach the agent
/// without a `crush.json` key. An empty render stays out of the owned paths, so a stale copy is
/// reconciled away as a ghost.
fn write_instructions(manifest: &MergedManifest, out: &Path) -> anyhow::Result<Option<PathBuf>> {
    crate::adapter::skills::reject_hardcoded_config_path(&manifest.agents_md, CRUSH_MD_FILE)?;
    if manifest.agents_md.trim().is_empty() {
        return Ok(None);
    }
    crate::paths::write_owner_only(&out.join(CRUSH_MD_FILE), manifest.agents_md.as_bytes())?;
    Ok(Some(PathBuf::from(CRUSH_MD_FILE)))
}

/// Project the skills of each plugin. A plugin with content that Crush cannot express is skipped
/// with a warning, so one plugin does not drop the other plugins (#543). The projected names feed
/// the collision check of the output styles, so that check reads the same state as this walk
/// (#1335).
fn project_plugins(
    manifest: &MergedManifest,
    out: &Path,
) -> anyhow::Result<(Vec<PathBuf>, Vec<ProjectedPluginSkill>)> {
    let mut paths = Vec::new();
    let mut projected = Vec::new();
    for plugin in &manifest.plugins {
        let payload = crate::adapter::resolve_plugin_payload(plugin, &manifest.marketplaces)?;
        if !plugin_is_compatible(&payload) {
            warn_unsupported_plugin(&plugin.plugin, &payload);
            continue;
        }
        let (plugin_paths, names) = crate::adapter::skills::project_plugin_skills(&payload, out)?;
        paths.extend(plugin_paths);
        projected.extend(names.into_iter().map(|skill_name| ProjectedPluginSkill {
            skill_name,
            plugin_name: plugin.plugin.clone(),
        }));
    }
    Ok((paths, projected))
}

fn warn_unsupported_plugin(plugin_name: &str, payload: &Path) {
    let bad_dir = UNSUPPORTED_PLUGIN_DIRS
        .iter()
        .find(|dir| payload.join(dir).is_dir())
        .unwrap_or(&"<unknown>");
    eprintln!(
        "warning: plugin '{plugin_name}' contains unsupported Crush content: '{bad_dir}/' \
         directory — skipping this plugin. Crush has no equivalent for plugin agents, commands, \
         or hooks. Scope this bundle away from Crush with `when:` or remove the content to \
         silence this warning."
    );
}

/// #1130: Crush has no output-style concept, so each style becomes a generated skill. The
/// collision check runs first, because the plugin skills exist on disk already (#1333).
fn write_output_styles(
    manifest: &MergedManifest,
    out: &Path,
    projected: &[ProjectedPluginSkill],
) -> anyhow::Result<Vec<PathBuf>> {
    let styles = &manifest.capabilities.output_styles;
    crate::adapter::output_styles::reject_plugin_skill_collisions(styles, projected)?;
    let mut owned = Vec::new();
    for style in styles {
        owned.extend(crate::adapter::output_styles::write_output_style_as_skill(
            out, style,
        )?);
    }
    Ok(owned)
}

/// The `crush.json` value: each section, then the `native.crush` layer on top.
fn build_doc(
    manifest: &MergedManifest,
    out: &Path,
    has_skills: bool,
    hooks: &[&crate::config::Hook],
) -> anyhow::Result<serde_json::Value> {
    let mut doc = Doc::new();
    insert(&mut doc, "hooks", render_hooks(manifest, hooks)?);
    // Crush has one permission key, `allowed_tools`. A tool named in `deny` or `ask` is withheld
    // from it, because an `allow` grant would otherwise override the rule (#1325). Omitting a
    // tool from the list is already fail-closed, and `disabled_tools` carries the deny.
    let (allowed_tools, disabled_tools) = render::render_permission_lists(
        &manifest.capabilities.permissions,
        manifest.capabilities.native_permissions.get("crush"),
    );
    if !allowed_tools.is_empty() {
        insert(
            &mut doc,
            "permissions",
            Some(json!({ "allowed_tools": allowed_tools })),
        );
    }
    insert(&mut doc, "mcp", mcp::render_mcp(manifest)?);
    insert(&mut doc, "lsp", render_lsp_section(manifest)?);
    add_model_sections(&mut doc, manifest)?;
    insert(
        &mut doc,
        "options",
        render::render_options(out, has_skills, &disabled_tools)?,
    );
    // Crush shell-expands each value at startup, so a literal `$` needs escaping.
    if !manifest.capabilities.env.is_empty() {
        insert(
            &mut doc,
            "env",
            Some(render::render_env(&manifest.capabilities.env)),
        );
    }
    apply_native(manifest, serde_json::Value::Object(doc))
}

fn insert(doc: &mut Doc, key: &str, value: Option<serde_json::Value>) {
    if let Some(value) = value {
        doc.insert(key.into(), value);
    }
}

/// An object with the native fragment on top, or `None` when it holds no key. A native-only
/// override can fill a section that has no modeled entry (#1031).
fn overlaid_object(
    mut value: serde_json::Value,
    native: Option<&serde_yaml::Value>,
    label: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    crate::adapter::overlay_native_json(&mut value, native, label)?;
    Ok(value
        .as_object()
        .is_some_and(|o| !o.is_empty())
        .then_some(value))
}

/// The `providers` and the `models` sections. A disabled provider is skipped, and an empty
/// section leaves its key out.
fn add_model_sections(doc: &mut Doc, manifest: &MergedManifest) -> anyhow::Result<()> {
    let caps = &manifest.capabilities;
    let providers = overlaid_object(
        render::render_model_providers(&caps.model_providers)?,
        caps.native_model_providers.get("crush"),
        "native_model_providers.crush",
    )?;
    insert(doc, "providers", providers);
    let models = overlaid_object(
        render_default_models(&caps.default_models),
        caps.native_default_models.get("crush"),
        "native_default_models.crush",
    )?;
    insert(doc, "models", models);
    Ok(())
}

/// The `lsp` object, or `None` when no enabled server remains.
fn render_lsp_section(manifest: &MergedManifest) -> anyhow::Result<Option<serde_json::Value>> {
    if manifest.capabilities.lsp.is_empty() {
        return Ok(None);
    }
    let value = render_lsp(&manifest.capabilities.lsp)?;
    Ok(value
        .as_object()
        .is_some_and(|o| !o.is_empty())
        .then_some(value))
}

/// The hooks that Crush can run. A bundle shared between engines often holds hooks that only
/// Claude Code can run. That is a gap between engines, not a mistake, so the hook is skipped with
/// a warning (#543).
fn compatible_hooks(manifest: &MergedManifest) -> Vec<&crate::config::Hook> {
    manifest
        .capabilities
        .hooks
        .iter()
        .filter(|hook| {
            crate::adapter::hook_support::engine_can_run_hook("Crush", SUPPORTED_HOOK_EVENTS, hook)
        })
        .collect()
}

/// The `hooks` object, or `None` when it holds no event.
///
/// Crush wants a flat `{ matcher?, command, name?, timeout? }` entry for each event, not the
/// nested `{ matcher, hooks: [...] }` form of Claude Code. The nested form gives an empty
/// `command`, and Crush rejects the whole config at load time.
fn render_hooks(
    manifest: &MergedManifest,
    compatible: &[&crate::config::Hook],
) -> anyhow::Result<Option<serde_json::Value>> {
    let mut by_event = render::render_hook_entries(compatible);
    let pre_tool_use = by_event.entry("PreToolUse".into()).or_default();
    // The guards of llmenv run on every tool call, as on Claude Code.
    pre_tool_use.push(json!({ "command": hook_command("hook-run --engine crush pre_tool_use")? }));
    // #4052: deny a write under /tmp. The command reads the tool name itself, so it takes no
    // matcher.
    pre_tool_use.push(json!({ "command": hook_command("tmp-guard --engine crush")? }));
    let mut value =
        serde_json::Value::Object(by_event.into_iter().map(|(k, v)| (k, json!(v))).collect());
    crate::adapter::overlay_native_json(
        &mut value,
        manifest.capabilities.native_hooks.get("crush"),
        "native_hooks.crush",
    )?;
    reject_unsupported_events(&value)?;
    Ok(value
        .as_object()
        .is_some_and(|o| !o.is_empty())
        .then_some(value))
}

/// `native_hooks.crush` can add an event that the manifest gate never saw (P1-4).
fn reject_unsupported_events(hooks: &serde_json::Value) -> anyhow::Result<()> {
    let Some(obj) = hooks.as_object() else {
        return Ok(());
    };
    if let Some(event) = obj
        .keys()
        .find(|event| !SUPPORTED_HOOK_EVENTS.contains(&event.as_str()))
    {
        anyhow::bail!(
            "native_hooks.crush contains unsupported hook event '{event}'. Supported events: {}. \
             Remove or move this hook.",
            SUPPORTED_HOOK_EVENTS.join(", ")
        );
    }
    Ok(())
}

/// The `native.crush` layer, the highest precedence. A modeled key is rejected first, because it
/// has its own rendering path, and the catch-all must not overwrite the security output (P1-3).
fn apply_native(
    manifest: &MergedManifest,
    mut doc: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let native = manifest.native.get("crush");
    render::reject_native_disabled_tools(native)?;
    if let Some(native) = native {
        crate::adapter::reject_modeled_native_keys(native, CRUSH_MODELED_KEYS, "crush")?;
    }
    crate::adapter::overlay_native_json(&mut doc, native, "native.crush")?;
    // #1270: a native null on a rendered key deletes the key, and does not persist as a JSON
    // null. It runs after the last overlay, so it covers every layer.
    crate::adapter::strip_json_nulls(&mut doc);
    Ok(doc)
}
