use std::path::Path;

use anyhow::Context;
use serde_json::json;

use super::AgentAdapter;
use crate::mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, ResolvedMcp};
use crate::merge::MergedManifest;
use crate::util::{dedup, merge_json};

/// Substitution value for `{{ICM_MCP}}` placeholders in bundle hook templates,
/// so bundle hooks can reference the memory MCP server by name without knowing
/// it ahead of time. Tracks the memory backend's registration name.
const ICM_MCP_NAME: &str = MEMORY_MCP_NAME;

/// Command the auto-emitted SessionStart hook runs (#121/#85). It shells back
/// into `llmenv` so the runtime check can compare the booted content hash (the
/// `CLAUDE_CONFIG_DIR` folder name the session launched with) against what
/// llmenv would materialize now, and warn the user to restart on drift. Kept as
/// a bare command (resolved off `PATH`) so it works regardless of install dir.
const STALE_CHECK_COMMAND: &str = "llmenv check-stale";

/// Adapter for Claude Code: writes `CLAUDE.md` (from `agents_md`) and copies
/// all merged files into `out`. Sets `CLAUDE_CONFIG_DIR` so Claude Code uses
/// `out` as its config root.
///
/// Skills are structured as directories with a `SKILL.md` file containing YAML
/// frontmatter (at minimum `name` and `description`).
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn env_vars(&self, cache_dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        Ok(vec![("CLAUDE_CONFIG_DIR".into(), dir.to_owned())])
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(out)?;
        crate::paths::write_owner_only(&out.join("CLAUDE.md"), manifest.agents_md.as_bytes())?;

        // Claude Code has a native rules-directory convention, so write each
        // `rules/*.md` file verbatim (frontmatter preserved) into `<out>/rules/`.
        // Adapters that lack this convention should instead use
        // `merge::agents_md::concat_with_rules` to inline the bodies.
        for r in &manifest.rules {
            if crate::paths::is_unsafe_join_target(r.rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in rules file: {}", r.rel.display());
            }
            let dest = out.join(&r.rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::paths::write_owner_only(&dest, r.raw.as_bytes())?;
        }

        // Copy all files from the manifest. JSON hook templates get
        // `{{ICM_MCP}}` substituted so bundle hooks can reference the MCP
        // server by name without hard-coding it.
        for (rel, abs) in &manifest.files {
            if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in bundle file: {}", rel.display());
            }
            let dest = out.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if is_hook_json(rel) {
                let raw = std::fs::read_to_string(abs)?;
                let rendered = raw.replace("{{ICM_MCP}}", ICM_MCP_NAME);
                crate::paths::write_owner_only(&dest, rendered.as_bytes())?;
            } else {
                std::fs::copy(abs, &dest)?;
            }
        }

        // Validate that skills are properly structured with SKILL.md frontmatter
        validate_skills(out)?;

        // Generate settings.json from hook/permission bundles
        generate_settings_json(out, manifest)?;

        // Emit mcp.json when the manifest carries resolved MCP servers or a
        // per-engine `native_mcp` fragment (#97) to overlay onto the doc.
        let native_mcp = manifest.capabilities.native_mcp.get("claude_code");
        if !manifest.mcps.is_empty() || native_mcp.is_some() {
            write_mcp_json(out, &manifest.mcps, native_mcp)?;
        }

        Ok(())
    }
}

/// Deep-merge a per-engine `native_*` fragment (opaque YAML) into an
/// already-built JSON config subtree. The fragment is converted to JSON and
/// overlaid via [`merge_json`], so it is the higher-precedence contributor
/// (native overrides win on scalar collision). A `None` fragment is a no-op.
fn overlay_native(
    dst: &mut serde_json::Value,
    fragment: Option<&serde_yaml::Value>,
) -> anyhow::Result<()> {
    if let Some(frag) = fragment {
        let as_json: serde_json::Value =
            serde_json::to_value(frag).context("converting native fragment to JSON")?;
        merge_json(dst, as_json);
    }
    Ok(())
}

/// Top-level settings.json keys that a modeled capability renders. The
/// top-level `native` catch-all (D3) is for keys NO modeled feature owns, so
/// these must never appear there — they belong in the `native_<feature>`
/// sibling, which merges in the safe direction (e.g. native deny can't weaken a
/// neutral deny). `enabledPlugins`/`extraKnownMarketplaces` (plugins) and the
/// separate `mcp.json` doc use distinct keys and aren't catch-all collisions.
const MODELED_SETTINGS_KEYS: [&str; 2] = ["permissions", "hooks"];

/// Reject a top-level `native.<engine>` catch-all fragment that contains a
/// modeled-feature key. Overlaying such a key last would silently clobber the
/// security-rendered output (see the call site). Returns an error naming the
/// offending key and pointing at the correct `native_<feature>` sibling.
fn reject_modeled_keys_in_catch_all(fragment: &serde_yaml::Value) -> anyhow::Result<()> {
    let Some(map) = fragment.as_mapping() else {
        return Ok(());
    };
    for key in MODELED_SETTINGS_KEYS {
        if map.contains_key(serde_yaml::Value::String(key.into())) {
            anyhow::bail!(
                "top-level `native.claude_code` carries the modeled-feature key \
                 `{key}`, which would silently clobber the rendered `{key}` \
                 (a security regression for permissions). Move it to the \
                 `native_{key}` sibling instead, which merges in the safe direction."
            );
        }
    }
    Ok(())
}

/// True when a `native_mcp` fragment already declares `enabledMcpjsonServers`.
/// When it does, llmenv skips its auto-derived approval list and defers entirely
/// to the user's curated set (#122) — for an approval list, native curation
/// replaces rather than unions.
fn native_sets_enabled_mcp(native: Option<&serde_yaml::Value>) -> bool {
    native
        .and_then(serde_yaml::Value::as_mapping)
        .is_some_and(|m| m.contains_key("enabledMcpjsonServers"))
}

/// True if `rel` is a JSON file under the bundle's `hooks/` subtree —
/// these files are template-rendered rather than byte-copied so bundle hooks
/// can reference the ICM MCP via `{{ICM_MCP}}`.
fn is_hook_json(rel: &Path) -> bool {
    rel.starts_with("hooks") && rel.extension().is_some_and(|e| e == "json")
}

/// Writes `mcp.json` registering every resolved MCP server under the
/// `mcpServers` key. Stdio entries carry `command`/`args`/`env`; remote entries
/// carry `url`. Entries are keyed by server name.
///
/// #97: a per-engine `native_mcp` fragment (mcp.json-shaped opaque YAML) is
/// overlaid onto the doc so engine-only keys (e.g. `enabledMcpjsonServers`)
/// merge in. Native is the higher-precedence overlay.
///
/// #103: Detects true same-identity-different-content conflicts: if two MCP
/// server definitions have the same name but different command/args/url,
/// hard-errors naming both contributors and the conflicting name. This prevents
/// silent overwrites that hide real mistakes.
fn write_mcp_json(
    out: &Path,
    mcps: &[ResolvedMcp],
    native: Option<&serde_yaml::Value>,
) -> anyhow::Result<()> {
    let mut servers = serde_json::Map::new();
    // Track which server came from which resolved entry for conflict reporting.
    let mut server_sources: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for (idx, m) in mcps.iter().enumerate() {
        let entry = match &m.kind {
            ResolvedKind::Stdio { command, args, env } => {
                let mut obj = json!({ "command": command, "args": args });
                if !env.is_empty() {
                    obj["env"] = json!(env);
                }
                obj
            }
            ResolvedKind::Remote { url, .. } => json!({ "url": url }),
        };

        // O3: Detect true same-identity-different-content conflicts.
        // If the server name already exists and the content differs, hard-error.
        if let Some(&prev_idx) = server_sources.get(&m.name)
            && let Some(existing_entry) = servers.get(&m.name)
            && existing_entry != &entry
        {
            anyhow::bail!(
                "true semantic conflict: MCP server '{}' defined twice with \
                 different content. First definition (entry #{}) differs from \
                 second definition (entry #{}). Resolve by removing or renaming \
                 one server definition.",
                m.name,
                prev_idx,
                idx,
            );
        }

        server_sources.insert(m.name.clone(), idx);
        servers.insert(m.name.clone(), entry);
    }
    // #122 (design O1, 'lean derive'): auto-approve every server llmenv emits by
    // deriving `enabledMcpjsonServers` from the resolved set. This spares users a
    // manual native passthrough just to trust servers llmenv itself wrote.
    //
    // A user who hand-curates the approval set via `native_mcp` wins outright:
    // we skip the derive when the native fragment already carries the key, rather
    // than letting `overlay_native`'s array union+dedup merge the two lists. For
    // an *approval* list, union is the wrong default — it would silently
    // re-approve a server the user deliberately left out. Native curation is a
    // replace, not an add.
    let mut doc = json!({ "mcpServers": servers });
    if !native_sets_enabled_mcp(native) {
        let enabled: Vec<String> = mcps.iter().map(|m| m.name.clone()).collect();
        if !enabled.is_empty() {
            doc["enabledMcpjsonServers"] = json!(enabled);
        }
    }
    overlay_native(&mut doc, native)?;
    let path = out.join("mcp.json");
    crate::paths::write_owner_only_atomic(&path, serde_json::to_string_pretty(&doc)?.as_bytes())?;
    Ok(())
}

/// Validates that all skills in the materialized directory have SKILL.md with required frontmatter.
fn validate_skills(out: &Path) -> anyhow::Result<()> {
    let skills_dir = out.join("skills");
    if !skills_dir.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&skills_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Skip non-directories
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            return Err(anyhow::anyhow!(
                "Skill directory {} missing SKILL.md",
                path.display()
            ));
        }

        let content = std::fs::read_to_string(&skill_md)?;

        if let Some(frontmatter_end) = content.find("\n---\n").or_else(|| {
            if content.ends_with("---") {
                Some(content.len() - 3)
            } else {
                None
            }
        }) {
            let frontmatter_str = &content[3..frontmatter_end];
            match serde_yaml::from_str::<serde_yaml::Mapping>(frontmatter_str) {
                Ok(mapping) => {
                    let has_name = mapping.get("name").is_some();
                    let has_description = mapping.get("description").is_some();

                    if !has_name || !has_description {
                        return Err(anyhow::anyhow!(
                            "Skill {} SKILL.md missing required frontmatter fields (name and description)",
                            path.display()
                        ));
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Skill {} SKILL.md has invalid YAML frontmatter: {}",
                        path.display(),
                        e
                    ));
                }
            }
        } else {
            return Err(anyhow::anyhow!(
                "Skill {} SKILL.md missing YAML frontmatter (must start with --- and end with ---)",
                path.display()
            ));
        }
    }

    Ok(())
}

/// Generates settings.json from the already-merged hook + permission
/// capabilities in the manifest.
///
/// Hooks (#90): `Vec<Hook>` → `{ EventName: [{ matcher?, hooks: [handler] }] }`.
///
/// Permissions (#34): neutral `{tool, pattern|paths}` rules render into Claude's
/// `Tool(pattern)` string grammar and land in `permissions.{allow,ask,deny}`
/// alongside the verbatim `native.claude_code` rule strings (one flat array per
/// action — not a nested `native` object). `default_mode` maps to `defaultMode`.
/// Native rules win in the safe direction only — deny is authoritative
/// (authority runs deny > ask > allow). A native `deny` suppresses a neutral
/// `allow`/`ask` of the same string, but a native `allow` never suppresses a
/// neutral `deny`: silently weakening a deny would be a security regression.
/// Cross-bundle merge (concat + dedup, scope-ordered) already happened in
/// [`crate::merge`]; this function only renders.
///
/// Resolve bundle-relative paths in a hook command string.
/// Scans whitespace-separated tokens and resolves those containing '/' (but not
/// starting with '/', '~', '$', or '-') to absolute paths relative to bundle_dir.
fn resolve_bundle_relative_paths(command: &str, bundle_dir: &Path) -> Option<String> {
    let mut resolved = false;
    let mut result = String::new();
    for (i, token) in command.split_whitespace().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        if token.contains('/')
            && !token.starts_with('/')
            && !token.starts_with('~')
            && !token.starts_with('$')
            && !token.starts_with('-')
        {
            let abs_path = bundle_dir.join(token);
            result.push_str(&abs_path.to_string_lossy());
            resolved = true;
        } else {
            result.push_str(token);
        }
    }
    if resolved { Some(result) } else { None }
}

/// SessionStart (#85): the hook object shape supports it; hash-comparison logic
/// lives in the runtime hook script.
fn generate_settings_json(out: &Path, manifest: &MergedManifest) -> anyhow::Result<()> {
    let mut settings = serde_json::Map::new();

    // #90: Transform hooks: Vec<Hook> into { EventName: [{ matcher, hooks: [...] }] }
    // Design: https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md
    let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for hook in &manifest.capabilities.hooks {
        // Resolve bundle-relative paths if this hook came from a bundle
        let resolved_command = if let Some(cmd) = &hook.handler.command {
            if let Some(bundle_dir) = &hook.bundle_origin {
                resolve_bundle_relative_paths(cmd, bundle_dir).or_else(|| Some(cmd.clone()))
            } else {
                Some(cmd.clone())
            }
        } else {
            None
        };

        let handler = json!({
            "command": resolved_command,
            "tool": hook.handler.tool,
            "type": match hook.handler.kind {
                crate::config::HookHandlerKind::Command => "command",
                crate::config::HookHandlerKind::McpTool => "mcp_tool",
            },
        });

        let mut hook_entry = serde_json::Map::new();
        if let Some(matcher) = &hook.matcher {
            hook_entry.insert("matcher".into(), json!(matcher));
        }
        hook_entry.insert("hooks".into(), json!([handler]));

        hooks_by_event
            .entry(hook.event.clone())
            .or_default()
            .push(serde_json::Value::Object(hook_entry));
    }

    // #121/#85: always register a SessionStart stale-context check. It concats
    // with any bundle- or native-declared SessionStart entries (events union),
    // so a user's own SessionStart hook is never clobbered. The runtime command
    // reads the booted hash off CLAUDE_CONFIG_DIR and recomputes the current one.
    hooks_by_event
        .entry("SessionStart".to_string())
        .or_default()
        .push(json!({
            "hooks": [{ "type": "command", "command": STALE_CHECK_COMMAND }],
        }));

    let mut hooks_obj = serde_json::Map::new();
    for (event, entries) in hooks_by_event {
        hooks_obj.insert(event, json!(entries));
    }
    // #97: overlay the per-engine `native_hooks` fragment (a `hooks`-shaped
    // settings.json object) so engine-only events and handlers merge in. Shared
    // events concat their entry arrays; native is the higher-precedence overlay.
    let mut hooks_value = serde_json::Value::Object(hooks_obj);
    overlay_native(
        &mut hooks_value,
        manifest.capabilities.native_hooks.get("claude_code"),
    )?;
    settings.insert("hooks".into(), hooks_value);

    // #34: Render neutral permission rules into Claude's string grammar
    // (`Tool(pattern)` / `Tool(path)` / bare `Tool`), then append the per-engine
    // `native.claude_code` rule strings verbatim into the same allow/ask/deny
    // arrays. Native rules are not a separate object — Claude Code reads one flat
    // array per action (see docs/reference/claude-code/permissions.md). They
    // share the array because both are just permission rule strings; the only
    // difference is neutral rules are generated and native ones are authored.
    let perms = &manifest.capabilities.permissions;
    let native = manifest.capabilities.native_permissions.get("claude_code");

    // Native rules win over neutral ones, but only in the safe direction: deny is
    // authoritative. Authority runs deny > ask > allow (most restrictive wins). A
    // neutral string is dropped only when a *more authoritative* native action
    // claims it — so a native `deny: ["WebFetch(domain:x)"]` suppresses a neutral
    // `allow`/`ask` of the same string (native deny wins), but a native `allow`
    // never suppresses a neutral `deny`. Silently weakening a deny would be a
    // security regression. Within the same action, agreeing native+neutral strings
    // simply dedupe (the native list is appended below).
    // Only deny and ask can outrank a neutral rule (deny > ask > allow), so a
    // native allow set is never a suppressor and isn't collected.
    let native_ask: std::collections::BTreeSet<&str> = native.map_or_else(Default::default, |n| {
        n.ask.iter().map(String::as_str).collect()
    });
    let native_deny: std::collections::BTreeSet<&str> = native.map_or_else(Default::default, |n| {
        n.deny.iter().map(String::as_str).collect()
    });

    // For a neutral rule in `action`, the set of native strings that outrank it.
    let suppressors = |action: PermissionAction| -> Vec<&std::collections::BTreeSet<&str>> {
        match action {
            PermissionAction::Allow => vec![&native_deny, &native_ask],
            PermissionAction::Ask => vec![&native_deny],
            PermissionAction::Deny => Vec::new(),
        }
    };

    let render_action = |neutral: &[crate::config::PermissionRule],
                         native_rules: &[String],
                         action: PermissionAction| {
        let outranking = suppressors(action);
        let mut out: Vec<String> = Vec::new();
        for rule in neutral {
            for s in render_permission_rule(rule) {
                // Drop the neutral string only when a more authoritative native
                // action asserts it — unless this action's own native list also
                // asserts it (appended below, so an agreeing pair still emits).
                let outranked = outranking.iter().any(|set| set.contains(s.as_str()));
                if outranked && !native_rules.contains(&s) {
                    continue;
                }
                out.push(s);
            }
        }
        out.extend(native_rules.iter().cloned());
        dedup(&mut out);
        out
    };

    let allow = render_action(
        &perms.allow,
        native.map_or(&[], |n| &n.allow),
        PermissionAction::Allow,
    );
    let ask = render_action(
        &perms.ask,
        native.map_or(&[], |n| &n.ask),
        PermissionAction::Ask,
    );
    let deny = render_action(
        &perms.deny,
        native.map_or(&[], |n| &n.deny),
        PermissionAction::Deny,
    );

    let has_perms =
        !allow.is_empty() || !ask.is_empty() || !deny.is_empty() || perms.default_mode.is_some();
    if has_perms {
        let mut perm_obj = serde_json::Map::new();
        if let Some(mode) = perms.default_mode {
            perm_obj.insert("defaultMode".into(), json!(permission_mode_str(mode)));
        }
        // Always emit the three arrays when any permission config exists, so the
        // shape matches Claude Code's object schema even if one action is empty.
        perm_obj.insert("allow".into(), json!(allow));
        perm_obj.insert("ask".into(), json!(ask));
        perm_obj.insert("deny".into(), json!(deny));
        settings.insert("permissions".into(), serde_json::Value::Object(perm_obj));
    }

    // #123 (design O4): when llmenv's ICM memory backend is active (the `icm`
    // MCP server is resolved), disable Claude's native auto memory so the two
    // memory systems don't compete. Emitted before the native overlays so a user
    // who explicitly sets `autoMemoryEnabled` via the top-level `native` catch-all
    // still wins (native is the higher-precedence layer, applied last).
    let icm_active = manifest.mcps.iter().any(|m| m.name == ICM_MCP_NAME);
    if icm_active {
        settings.insert("autoMemoryEnabled".into(), json!(false));
    }

    // Plugins (#59): declare marketplaces + enabled plugins into settings.json.
    // llmenv owns the marketplace clone in its cache, so each marketplace points
    // Claude at that checkout via a `directory` source (no re-fetch). Plugins are
    // keyed `<plugin>@<marketplace>` and force-enabled.
    render_plugins(&mut settings, manifest);

    // #97: overlay the per-engine `native_plugins` fragment at the settings top
    // level (plugin-related keys Claude understands but llmenv has no neutral
    // form for, e.g. extra `enabledPlugins` entries).
    let mut settings_value = serde_json::Value::Object(settings);
    overlay_native(
        &mut settings_value,
        manifest.capabilities.native_plugins.get("claude_code"),
    )?;

    // #96: overlay the top-level `native.claude_code` catch-all last — opaque
    // keys that belong to no modeled feature (e.g. `alwaysThinkingEnabled`).
    // It is the highest-precedence layer, applied after every modeled render.
    //
    // Security guard (#102): the catch-all is for keys NO modeled feature owns.
    // A modeled-feature key here (`permissions`, `hooks`) would overlay LAST over
    // the security-rendered output, silently clobbering it — e.g. erasing the
    // permission `deny` array, bypassing the deny-never-weakened invariant. Per
    // design D3 ("Layer 1 wins, or hard-error"), reject it loudly. The key
    // belongs in the `native_<feature>` sibling, which merges in the safe
    // direction.
    if let Some(native) = manifest.native.get("claude_code") {
        reject_modeled_keys_in_catch_all(native)?;
    }
    overlay_native(&mut settings_value, manifest.native.get("claude_code"))?;
    let settings_path = out.join("settings.json");
    let json_str = serde_json::to_string_pretty(&settings_value)?;

    crate::paths::write_owner_only_atomic(&settings_path, json_str.as_bytes()).with_context(
        || {
            format!(
                "Failed to write settings.json at {}",
                settings_path.display()
            )
        },
    )?;

    Ok(())
}

/// Render the manifest's resolved marketplaces + plugins into `settings`.
///
/// - `extraKnownMarketplaces`: keyed by marketplace name. Source is `directory`
///   pointing at llmenv's local clone (`install_location`) so Claude loads the
///   already-synced checkout instead of re-fetching. A marketplace with no
///   install location (never synced) is skipped.
/// - `enabledPlugins`: keyed `<plugin>@<marketplace>`, all `true`. llmenv only
///   emits plugins it wants on; it never authors a `false` (disabled) entry.
///
/// Both keys are omitted entirely when empty so a plugin-free scope produces no
/// plugin settings.
fn render_plugins(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    manifest: &MergedManifest,
) {
    if manifest.marketplaces.is_empty() && manifest.plugins.is_empty() {
        return;
    }

    let mut markets = serde_json::Map::new();
    for mk in &manifest.marketplaces {
        let Some(location) = &mk.install_location else {
            continue;
        };
        markets.insert(
            mk.name.clone(),
            json!({
                "source": { "source": "directory", "path": location },
            }),
        );
    }
    if !markets.is_empty() {
        settings.insert(
            "extraKnownMarketplaces".into(),
            serde_json::Value::Object(markets),
        );
    }

    let mut enabled = serde_json::Map::new();
    for p in &manifest.plugins {
        enabled.insert(format!("{}@{}", p.plugin, p.marketplace), json!(true));
    }
    if !enabled.is_empty() {
        settings.insert("enabledPlugins".into(), serde_json::Value::Object(enabled));
    }
}

/// Render a neutral permission rule into Claude Code's string grammar.
///
/// - `{tool: Bash, pattern: "cargo *"}` → `["Bash(cargo *)"]`
/// - `{tool: Read, paths: ["./.env", "./.env.*"]}` → `["Read(./.env)", "Read(./.env.*)"]`
///   (one string per path — Claude has no multi-path rule form).
/// - `{tool: Bash}` (no pattern, no paths) → `["Bash"]` (tool-wide rule).
///
/// `pattern` and `paths` are mutually exclusive by the neutral schema's
/// intent; if both are somehow set, `pattern` wins and `paths` is ignored — the
/// neutral form documents pattern as the scalar case.
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

/// Map the neutral `PermissionMode` onto Claude Code's `defaultMode` string.
fn permission_mode_str(mode: crate::config::PermissionMode) -> &'static str {
    use crate::config::PermissionMode;
    match mode {
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Default => "default",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// Which permission action a neutral rule belongs to. Authority for native-wins
/// suppression runs deny > ask > allow (most restrictive wins), so a neutral
/// rule is only ever suppressed by a native rule in a *more* authoritative
/// action — a native deny can suppress a neutral allow, never the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        MODELED_SETTINGS_KEYS, is_hook_json, overlay_native, reject_modeled_keys_in_catch_all,
        render_permission_rule, write_mcp_json,
    };
    use crate::config::PermissionRule;
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use proptest::prelude::*;
    use std::path::PathBuf;

    proptest! {
        // A rule with a `pattern` always renders to exactly one `Tool(pattern)`
        // string, regardless of any `paths` (pattern wins per the neutral schema).
        #[test]
        fn pattern_renders_single_tool_pattern_string(
            tool in "[A-Za-z]{1,12}",
            pattern in "[^()]{0,20}",
            paths in proptest::collection::vec("[^()]{0,10}", 0..3),
        ) {
            let rule = PermissionRule { tool: tool.clone(), pattern: Some(pattern.clone()), paths };
            let out = render_permission_rule(&rule);
            prop_assert_eq!(out, vec![format!("{tool}({pattern})")]);
        }

        // With no pattern, each path yields one `Tool(path)` string, in order.
        #[test]
        fn paths_render_one_string_each_in_order(
            tool in "[A-Za-z]{1,12}",
            paths in proptest::collection::vec("[^()]{1,10}", 1..5),
        ) {
            let rule = PermissionRule { tool: tool.clone(), pattern: None, paths: paths.clone() };
            let out = render_permission_rule(&rule);
            let expected: Vec<String> = paths.iter().map(|p| format!("{tool}({p})")).collect();
            prop_assert_eq!(out, expected);
        }

        // No pattern and no paths → a bare tool-wide rule.
        #[test]
        fn bare_tool_renders_tool_name(tool in "[A-Za-z]{1,12}") {
            let rule = PermissionRule { tool: tool.clone(), pattern: None, paths: Vec::new() };
            prop_assert_eq!(render_permission_rule(&rule), vec![tool]);
        }

        // Rendering is deterministic: same input, same output, never panics.
        #[test]
        fn rendering_is_deterministic(
            tool in "[A-Za-z]{1,12}",
            pattern in proptest::option::of("[^()]{0,20}"),
            paths in proptest::collection::vec("[^()]{0,10}", 0..4),
        ) {
            let rule = PermissionRule { tool, pattern, paths };
            prop_assert_eq!(render_permission_rule(&rule), render_permission_rule(&rule));
        }

        // #107 overlay_native: a `None` fragment leaves the destination untouched.
        #[test]
        fn overlay_native_none_is_noop(seed in 0u64..1000) {
            let mut dst = serde_json::json!({ "k": seed, "nested": { "a": [1, 2] } });
            let before = dst.clone();
            overlay_native(&mut dst, None).unwrap();
            prop_assert_eq!(dst, before);
        }

        // #107 overlay_native idempotence: overlaying the same fragment twice
        // equals overlaying it once, for ANY fragment. merge_json normalizes
        // arrays on every path (insert and recursive-merge alike), so a
        // duplicate-laden source array is deduped on first overlay and the
        // second overlay is a no-op.
        #[test]
        fn overlay_native_is_idempotent(frag in arb_yaml_value(3)) {
            let mut base = serde_json::json!({ "existing": "value", "list": ["x"] });
            let mut once = base.clone();
            overlay_native(&mut once, Some(&frag)).unwrap();
            overlay_native(&mut base, Some(&frag)).unwrap();
            overlay_native(&mut base, Some(&frag)).unwrap();
            prop_assert_eq!(base, once);
        }

        // #107 overlay_native no-crash: arbitrary YAML never panics and the
        // converted fragment's own keys win on scalar collision (native is the
        // higher-precedence overlay).
        #[test]
        fn overlay_native_never_panics(frag in arb_yaml_value(4)) {
            let mut dst = serde_json::json!({});
            // Must not panic regardless of fragment shape.
            let _ = overlay_native(&mut dst, Some(&frag));
        }

        // #109 reject_modeled_keys: a fragment that is not a mapping (scalar,
        // sequence, null) is always accepted — there are no top-level keys to
        // collide with a modeled feature.
        #[test]
        fn reject_modeled_keys_accepts_non_mappings(frag in arb_non_mapping_yaml()) {
            prop_assert!(reject_modeled_keys_in_catch_all(&frag).is_ok());
        }

        // #109 reject_modeled_keys acceptance: a mapping built only from keys that
        // are NOT modeled-feature keys always passes.
        #[test]
        fn reject_modeled_keys_accepts_unmodeled_mappings(
            keys in proptest::collection::vec("[a-z]{1,10}", 0..6),
        ) {
            let mut map = serde_yaml::Mapping::new();
            for k in keys {
                if MODELED_SETTINGS_KEYS.contains(&k.as_str()) {
                    continue; // never inject a modeled key in this acceptance case
                }
                map.insert(serde_yaml::Value::String(k), serde_yaml::Value::Bool(true));
            }
            let frag = serde_yaml::Value::Mapping(map);
            prop_assert!(reject_modeled_keys_in_catch_all(&frag).is_ok());
        }

        // #109 reject_modeled_keys rejection completeness: a mapping containing ANY
        // modeled key is always rejected, regardless of other keys present.
        #[test]
        fn reject_modeled_keys_rejects_any_modeled_key(
            modeled_idx in 0usize..MODELED_SETTINGS_KEYS.len(),
            extra_keys in proptest::collection::vec("[a-z]{1,8}", 0..4),
        ) {
            let mut map = serde_yaml::Mapping::new();
            for k in extra_keys {
                map.insert(serde_yaml::Value::String(k), serde_yaml::Value::Null);
            }
            let modeled = MODELED_SETTINGS_KEYS[modeled_idx];
            map.insert(
                serde_yaml::Value::String(modeled.to_owned()),
                serde_yaml::Value::Null,
            );
            let frag = serde_yaml::Value::Mapping(map);
            let err = reject_modeled_keys_in_catch_all(&frag);
            prop_assert!(err.is_err());
            prop_assert!(err.unwrap_err().to_string().contains(modeled));
        }

        // #110 is_hook_json correctness: returns true iff the path starts with the
        // `hooks` component AND has a `.json` extension. Built from components so
        // the property holds across separators and arbitrary names.
        #[test]
        fn is_hook_json_matches_spec(
            first in "[a-z]{1,8}",
            mid in proptest::collection::vec("[a-z]{1,6}", 0..3),
            stem in "[a-z]{1,8}",
            ext in proptest::option::of("[a-z]{1,5}"),
        ) {
            let mut p = PathBuf::from(&first);
            for c in &mid {
                p.push(c);
            }
            let file = match &ext {
                Some(e) => format!("{stem}.{e}"),
                None => stem.clone(),
            };
            p.push(&file);

            let expected = first == "hooks" && ext.as_deref() == Some("json");
            prop_assert_eq!(is_hook_json(&p), expected);
        }

        // #110 is_hook_json determinism + no-panic: arbitrary path strings
        // (including special chars) classify consistently and never panic.
        #[test]
        fn is_hook_json_is_deterministic(raw in ".{0,40}") {
            let p = PathBuf::from(&raw);
            prop_assert_eq!(is_hook_json(&p), is_hook_json(&p));
        }

        // #108 write_mcp_json producibility + roundtrip: every distinctly-named
        // resolved MCP appears under `mcpServers` in valid, re-parseable JSON.
        #[test]
        fn write_mcp_json_roundtrips_distinct_servers(mcps in arb_distinct_mcps()) {
            let dir = tempfile::tempdir().unwrap();
            write_mcp_json(dir.path(), &mcps, None).unwrap();

            let raw = std::fs::read_to_string(dir.path().join("mcp.json")).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let servers = doc.get("mcpServers").and_then(|v| v.as_object()).unwrap();

            prop_assert_eq!(servers.len(), mcps.len());
            for m in &mcps {
                let entry = servers.get(&m.name).unwrap();
                match &m.kind {
                    ResolvedKind::Stdio { command, args, env } => {
                        prop_assert_eq!(entry.get("command").unwrap(), command);
                        // args always serialize as an array (possibly empty).
                        let got_args: Vec<&str> = entry
                            .get("args")
                            .and_then(|v| v.as_array())
                            .unwrap()
                            .iter()
                            .map(|v| v.as_str().unwrap())
                            .collect();
                        prop_assert_eq!(got_args, args.iter().map(String::as_str).collect::<Vec<_>>());
                        // env is present iff non-empty; when present, every pair
                        // round-trips.
                        if env.is_empty() {
                            prop_assert!(entry.get("env").is_none());
                        } else {
                            let got_env = entry.get("env").and_then(|v| v.as_object()).unwrap();
                            prop_assert_eq!(got_env.len(), env.len());
                            for (k, v) in env {
                                prop_assert_eq!(got_env.get(k).unwrap().as_str().unwrap(), v);
                            }
                        }
                    }
                    ResolvedKind::Remote { url, .. } => {
                        prop_assert_eq!(entry.get("url").unwrap(), url);
                    }
                }
            }
        }

        // #108 write_mcp_json overlay determinism: an empty native overlay onto a
        // server doc is a deterministic no-op on the `mcpServers` content.
        #[test]
        fn write_mcp_json_empty_overlay_is_deterministic(mcps in arb_distinct_mcps()) {
            let empty = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

            let dir_a = tempfile::tempdir().unwrap();
            write_mcp_json(dir_a.path(), &mcps, Some(&empty)).unwrap();
            let a = std::fs::read_to_string(dir_a.path().join("mcp.json")).unwrap();

            let dir_b = tempfile::tempdir().unwrap();
            write_mcp_json(dir_b.path(), &mcps, Some(&empty)).unwrap();
            let b = std::fs::read_to_string(dir_b.path().join("mcp.json")).unwrap();

            prop_assert_eq!(a, b);
        }

        // #150: write_mcp_json must produce a file with mode 0o600 — same
        // owner-only invariant as ICM state and settings.json. Critical
        // because mcp.json may contain server credentials / API URLs.
        #[cfg(unix)]
        #[test]
        fn write_mcp_json_writes_owner_only_permissions(mcps in arb_distinct_mcps()) {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            write_mcp_json(dir.path(), &mcps, None).unwrap();
            let mode = std::fs::metadata(dir.path().join("mcp.json"))
                .unwrap()
                .permissions()
                .mode();
            prop_assert_eq!(mode & 0o077, 0, "group/other bits set: {:o}", mode);
        }

        // #151: write_mcp_json output round-trips through serde_json — every
        // byte written deserializes back to a parsable Value with the same
        // mcpServers content. Catches drift between serialize and deserialize
        // paths.
        #[test]
        fn write_mcp_json_serde_roundtrip(mcps in arb_distinct_mcps()) {
            let dir = tempfile::tempdir().unwrap();
            write_mcp_json(dir.path(), &mcps, None).unwrap();
            let raw = std::fs::read_to_string(dir.path().join("mcp.json")).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse");
            // Reserialize and reparse — must produce identical structure.
            let reserialized = serde_json::to_string_pretty(&doc).expect("reserialize");
            let doc2: serde_json::Value = serde_json::from_str(&reserialized).expect("reparse");
            prop_assert_eq!(doc, doc2);
        }
    }

    // Recursively-shaped arbitrary YAML for fragment fuzzing. Bounded depth keeps
    // generation cheap while still exercising nested mappings/sequences.
    fn arb_yaml_value(depth: u32) -> impl Strategy<Value = serde_yaml::Value> {
        let leaf = prop_oneof![
            Just(serde_yaml::Value::Null),
            any::<bool>().prop_map(serde_yaml::Value::Bool),
            any::<i64>().prop_map(|n| serde_yaml::Value::Number(n.into())),
            "[a-z]{0,8}".prop_map(serde_yaml::Value::String),
        ];
        leaf.prop_recursive(depth, 16, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4)
                    .prop_map(serde_yaml::Value::Sequence),
                proptest::collection::vec(("[a-z]{1,6}", inner), 0..4).prop_map(|pairs| {
                    let mut m = serde_yaml::Mapping::new();
                    for (k, v) in pairs {
                        m.insert(serde_yaml::Value::String(k), v);
                    }
                    serde_yaml::Value::Mapping(m)
                }),
            ]
        })
    }

    // Arbitrary YAML that is never a top-level mapping (the early-return path of
    // reject_modeled_keys_in_catch_all).
    fn arb_non_mapping_yaml() -> impl Strategy<Value = serde_yaml::Value> {
        prop_oneof![
            Just(serde_yaml::Value::Null),
            any::<bool>().prop_map(serde_yaml::Value::Bool),
            any::<i64>().prop_map(|n| serde_yaml::Value::Number(n.into())),
            "[a-z]{0,8}".prop_map(serde_yaml::Value::String),
            proptest::collection::vec("[a-z]{0,6}".prop_map(serde_yaml::Value::String), 0..4)
                .prop_map(serde_yaml::Value::Sequence),
        ]
    }

    // A vector of ResolvedMcp with unique names (write_mcp_json hard-errors on
    // same-name-different-content, so the roundtrip properties require distinct
    // names to stay in the success path).
    fn arb_distinct_mcps() -> impl Strategy<Value = Vec<ResolvedMcp>> {
        proptest::collection::vec(arb_mcp(), 0..5).prop_map(|mcps| {
            let mut seen = std::collections::BTreeSet::new();
            mcps.into_iter()
                .filter(|m| seen.insert(m.name.clone()))
                .collect()
        })
    }

    fn arb_mcp() -> impl Strategy<Value = ResolvedMcp> {
        let stdio = (
            "[a-z][a-z0-9_-]{0,10}",
            "[a-z]{1,8}",
            proptest::collection::vec("[a-z]{0,6}", 0..3),
            // Sometimes empty, sometimes populated — exercises both the
            // env-omitted and env-serialized branches of write_mcp_json.
            proptest::collection::btree_map("[A-Z][A-Z_]{0,5}", "[a-z0-9]{0,8}", 0..3),
        )
            .prop_map(|(name, command, args, env)| ResolvedMcp {
                name,
                kind: ResolvedKind::Stdio { command, args, env },
            });
        let remote =
            ("[a-z][a-z0-9_-]{0,10}", "https://[a-z]{1,8}\\.test").prop_map(|(name, url)| {
                ResolvedMcp {
                    name,
                    kind: ResolvedKind::Remote {
                        url,
                        transport: crate::config::McpTransport::Http,
                    },
                }
            });
        prop_oneof![stdio, remote]
    }
}
