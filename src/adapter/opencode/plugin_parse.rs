//! Parses a plugin hooks manifest and its MCP entries into llmenv types.

use std::path::Path;

use super::PluginHooksManifest;
use crate::adapter::yaml_value_kind_name;

#[cfg(test)]
mod tests;

/// Parse a plugin hooks manifest (hooks.json or claude-codex-hooks.json) into
/// [`crate::config::Hook`] structs.
///
/// Resolves `${CLAUDE_PLUGIN_ROOT}` to `payload_path` and only emits
/// `command`-type hooks — other types are skipped with an actionable warning.
pub(super) fn parse_plugin_hooks(
    hooks_path: &Path,
    payload_path: &Path,
    plugin_name: &str,
) -> anyhow::Result<Option<Vec<crate::config::Hook>>> {
    // #915: `Ok(None)` means a manifest of this name is absent — the caller
    // tries the next candidate name. A real read error (e.g. permission
    // denied) propagates rather than being masked by a caller `exists()` guard.
    let source =
        match llmenv_paths::read_text_bounded(hooks_path, crate::paths::bounded::MAX_ASSET_BYTES) {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(None),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("reading {}", hooks_path.display()))
                );
            }
        };
    let manifest: PluginHooksManifest = serde_json::from_str(&source)?;

    let mut hooks = Vec::new();
    for (event, groups) in &manifest.hooks {
        for group in groups {
            for entry in &group.hooks {
                let kind = entry.kind.as_deref().unwrap_or("command");
                if kind != "command" {
                    eprintln!(
                        "warning: plugin '{plugin_name}' hooks manifest '{}': hook type '{kind}' \
                         is not supported by the opencode adapter — skipping. Only 'command' \
                         hooks are supported.",
                        hooks_path.display()
                    );
                    continue;
                }
                let Some(raw_command) = &entry.command else {
                    eprintln!(
                        "warning: plugin '{plugin_name}' hooks manifest '{}': a 'command' hook \
                         entry for event '{event}' is missing its 'command' field — skipping.",
                        hooks_path.display()
                    );
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
                        on_failure: None,
                    },
                    bundle_origin: None,
                });
            }
        }
    }

    Ok(Some(hooks))
}

/// Read and parse a plugin's `LLM_PROVIDER_MCP_JSON` file into MCP server
/// entries keyed by server name.
///
/// Isolated into its own fallible function so one plugin's malformed or
/// unreadable MCP config only drops that plugin's MCP servers (with a
/// warning) rather than aborting `materialize` for every other plugin.
pub(super) fn parse_plugin_mcp_entries(
    mcp_json_path: &Path,
    plugin_name: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    // #915: a missing file means the plugin ships no MCP config — nothing to
    // translate. Other read errors (e.g. permission denied) propagate rather
    // than being masked by a caller `exists()` guard.
    let content = match llmenv_paths::read_text_bounded(
        mcp_json_path,
        crate::paths::bounded::MAX_ASSET_BYTES,
    ) {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(serde_json::Map::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "plugin '{plugin_name}': failed to read {mcp_json_path:?}: {e}"
            ));
        }
    };
    let yaml_value: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| {
        anyhow::anyhow!("plugin '{plugin_name}': failed to parse LLM_PROVIDER_MCP_JSON: {e}")
    })?;
    let obj = yaml_value.as_mapping().ok_or_else(|| {
        anyhow::anyhow!(
            "plugin '{plugin_name}': LLM_PROVIDER_MCP_JSON must be a mapping of server name to \
             config, got {}",
            yaml_value_kind_name(&yaml_value)
        )
    })?;
    // Unreachable in practice: a serde_yaml::Mapping always serializes to a
    // serde_json::Value::Object (a YAML mapping with a non-string/complex key
    // fails the `to_value` call above instead, since JSON object keys must be
    // strings). Kept as an explicit error rather than `.unwrap()` so a future
    // serde_json behavior change degrades to a message, not a panic.
    let json_value: serde_json::Value = serde_json::to_value(obj)?;
    json_value.as_object().cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "plugin '{plugin_name}': LLM_PROVIDER_MCP_JSON mapping did not convert to a JSON object"
        )
    })
}
