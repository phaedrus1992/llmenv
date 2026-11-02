//! Renders the `crush.json` fragments that come from llmenv manifest capabilities.
//!
//! `crush.rs` merges these fragments into the rendered config. Each function is
//! one capability, so a change to one model key does not touch the others.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::util::dedup;

pub(super) fn render_lsp(
    servers: &[llmenv_config::LspServer],
) -> anyhow::Result<serde_json::Value> {
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
pub(super) fn render_model_providers(
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
pub(super) fn render_default_models(
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
pub(super) const CRUSH_MODELED_KEYS: &[&str] = &[
    "permissions",
    "hooks",
    "mcp",
    "lsp",
    "providers",
    "models",
    "env",
];

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
///
/// #1371: the mapping itself now lives in [`crate::adapter::tools`] alongside
/// opencode's — the two hand-written tables had drifted (crush treated
/// `MultiEdit` as its own tool while opencode folded it into `edit`, and neither
/// recorded whether an absent entry meant "no analog" or "not done yet"). This
/// is the crush column of that one table.
pub(super) fn crush_tool_name(neutral: &str) -> Option<&'static str> {
    crate::adapter::tools::crush_key(neutral)
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
pub(super) fn render_permission_rule(rule: &crate::config::PermissionRule) -> Vec<String> {
    if let Some(pattern) = &rule.pattern {
        crate::agent_notice::record_logged(format!(
            "crush: allowed_tools has no pattern matching; rule `{}` + pattern `{pattern}` \
             cannot be expressed for Crush and is dropped rather than widened to allow \
             ALL `{}` calls — this tool will still prompt for approval",
            rule.tool, rule.tool
        ));
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

/// Split the neutral permission rules into Crush's `allowed_tools` and `disabled_tools`.
///
/// A tool named by an `ask` or `deny` rule is withheld from `allowed_tools`, so it still
/// prompts. Crush has no other way to enforce a conflicting deny.
pub(super) fn render_permission_lists(
    perms: &crate::config::Permissions,
    native: Option<&crate::config::NativePermissionRules>,
) -> (Vec<String>, Vec<String>) {
    let mut allowed = render_rules_to_strings(&perms.allow);
    let mut withheld: BTreeSet<String> = perms
        .deny
        .iter()
        .chain(&perms.ask)
        .filter_map(|rule| crush_tool_name(&rule.tool))
        .map(str::to_string)
        .collect();
    // Crush's `edit` and `multiedit` also create missing files, so a `Write` deny needs their grants withheld.
    if perms.deny.iter().any(|rule| rule.tool == "Write") {
        withheld.extend(["edit".to_string(), "multiedit".to_string()]);
    }
    let mut disabled = render_disabled_tools(&perms.deny);
    if let Some(n) = native {
        allowed.extend(n.allow.iter().cloned());
        withheld.extend(n.ask.iter().cloned());
        withheld.extend(n.deny.iter().cloned());
        disabled.extend(n.deny.iter().cloned());
    }
    allowed.retain(|t| !withheld.contains(t));
    dedup(&mut allowed);
    dedup(&mut disabled);
    (allowed, disabled)
}

fn render_disabled_tools(deny: &[crate::config::PermissionRule]) -> Vec<String> {
    let mut disabled = Vec::new();
    for rule in deny {
        if rule.pattern.is_some() || !rule.paths.is_empty() {
            tracing::error!(
                "crush: deny rule `{}` is scoped to a pattern or paths, which Crush cannot \
                 express. The rule is not rendered, so `{}` stays available, with its prompt.",
                rule.tool,
                rule.tool
            );
            continue;
        }
        match crush_tool_name(&rule.tool) {
            Some(name) => {
                disabled.push(name.to_string());
                disabled.extend(crush_aliases(name).iter().map(|alias| alias.to_string()));
            }
            None => tracing::error!(
                "crush: no equivalent tool for neutral deny rule `{}`, so the rule cannot take \
                 effect on Crush. Fix: write a Crush deny in native_permissions.crush.deny, or \
                 remove the rule.",
                rule.tool
            ),
        }
    }
    dedup(&mut disabled);
    disabled
}

/// Crush tools that a neutral tool also reaches: `multiedit` edits files like `edit`, and
/// `agentic_fetch` fetches URLs like `fetch`. A deny on the neutral tool covers them too.
fn crush_aliases(crush_name: &str) -> &'static [&'static str] {
    match crush_name {
        "edit" => &["multiedit"],
        "fetch" => &["agentic_fetch"],
        _ => &[],
    }
}

pub(super) fn render_options(
    out: &Path,
    has_skills: bool,
    disabled_tools: &[String],
) -> anyhow::Result<Option<serde_json::Value>> {
    let mut options = serde_json::Map::new();
    if has_skills {
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
        options.insert("skills_paths".into(), json!([skills_out]));
    }
    if !disabled_tools.is_empty() {
        options.insert("disabled_tools".into(), json!(disabled_tools));
    }
    Ok((!options.is_empty()).then_some(serde_json::Value::Object(options)))
}

/// Builds the top-level `env` object. Crush expands each value as a shell word at startup, so
/// `$` and backticks are escaped. Without the escape, `$(...)` in a bundle value runs a command
/// when Crush starts, while the other engines read the same value literally.
pub(super) fn render_env(env: &std::collections::BTreeMap<String, String>) -> serde_json::Value {
    let escaped: serde_json::Map<String, serde_json::Value> = env
        .iter()
        .map(|(key, value)| (key.clone(), json!(escape_for_crush_expansion(value))))
        .collect();
    serde_json::Value::Object(escaped)
}

/// Escapes a value for Crush's document-style shell expansion. Single quotes are not special there.
fn escape_for_crush_expansion(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// `options.disabled_tools` carries the rendered deny rules. A native overlay on `options`
/// replaces or deletes that key without an error, so the native fragment may not set it.
pub(super) fn reject_native_disabled_tools(
    native: Option<&serde_yaml::Value>,
) -> anyhow::Result<()> {
    let sets_disabled = native
        .and_then(|n| n.get("options"))
        .and_then(|o| o.get("disabled_tools"))
        .is_some();
    anyhow::ensure!(
        !sets_disabled,
        "native.crush sets options.disabled_tools, which llmenv renders from the deny rules. \
         Remove that key from native.crush. Write the deny in permissions.deny, or in \
         native_permissions.crush.deny."
    );
    Ok(())
}

/// Builds the event-keyed hook entries for Crush. Crush's `HookConfig` is a flat object, so
/// each entry carries `matcher` and `command` directly. Claude Code nests them under `hooks`.
pub(super) fn render_hook_entries(
    hooks: &[&crate::config::Hook],
) -> std::collections::BTreeMap<String, Vec<serde_json::Value>> {
    let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for hook in hooks {
        // P2-6: mcp_tool hooks are filtered out at the gate above, so `command`
        // is always present for the remaining Command-kind hooks.
        let resolved_command = hook
            .handler
            .command
            .as_ref()
            .map(|cmd| match &hook.bundle_origin {
                Some(bundle_dir) => crate::adapter::hook_command::resolve_bundle_relative_paths(
                    cmd, bundle_dir,
                )
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
    hooks_by_event
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
