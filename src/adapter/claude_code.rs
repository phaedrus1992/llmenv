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
        std::fs::write(out.join("CLAUDE.md"), &manifest.agents_md)?;

        // Claude Code has a native rules-directory convention, so write each
        // `rules/*.md` file verbatim (frontmatter preserved) into `<out>/rules/`.
        // Adapters that lack this convention should instead use
        // `merge::agents_md::concat_with_rules` to inline the bodies.
        for r in &manifest.rules {
            let dest = out.join(&r.rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &r.raw)?;
        }

        // Copy all files from the manifest. JSON hook templates get
        // `{{ICM_MCP}}` substituted so bundle hooks can reference the MCP
        // server by name without hard-coding it.
        for (rel, abs) in &manifest.files {
            let dest = out.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if is_hook_json(rel) {
                let raw = std::fs::read_to_string(abs)?;
                let rendered = raw.replace("{{ICM_MCP}}", ICM_MCP_NAME);
                std::fs::write(&dest, rendered)?;
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
    let mut doc = json!({ "mcpServers": servers });
    overlay_native(&mut doc, native)?;
    let path = out.join("mcp.json");
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
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
/// SessionStart (#85): the hook object shape supports it; hash-comparison logic
/// lives in the runtime hook script.
fn generate_settings_json(out: &Path, manifest: &MergedManifest) -> anyhow::Result<()> {
    let mut settings = serde_json::Map::new();

    // #90: Transform hooks: Vec<Hook> into { EventName: [{ matcher, hooks: [...] }] }
    // Design: https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md
    let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for hook in &manifest.capabilities.hooks {
        let handler = json!({
            "command": hook.handler.command,
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

    std::fs::write(&settings_path, &json_str).with_context(|| {
        format!(
            "Failed to write settings.json at {}",
            settings_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&settings_path, perms).with_context(|| {
            format!(
                "Failed to set permissions on settings.json at {}",
                settings_path.display()
            )
        })?;
    }

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
mod tests {
    use super::render_permission_rule;
    use crate::config::PermissionRule;
    use proptest::prelude::*;

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
    }
}
