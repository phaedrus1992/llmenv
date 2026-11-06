//! The opencode 2.x hook bridge: the static plugin `plugin/llmenv.js` and its hook table
//! `plugin/llmenv-hooks.json` (#2840).
//!
//! opencode 2.0 loads a plugin only from a default export with an `id` and a `setup` function.
//! The plugin is a fixed asset, so the per-config part is the JSON table beside it. opencode
//! loads only `.js` and `.ts` files from `plugin/`, so the table is not read as a plugin.

use std::path::{Path, PathBuf};

use crate::adapter::hook_program::hook_command;

/// opencode supports exactly these hook events via its JS plugin API.
/// See spec §3 for the event mapping.
pub(super) const SUPPORTED_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "SubagentStart",
    "SubagentStop",
    "Notification",
    "PreCompact",
    "PostModelSwitch",
    "PostToolBatch",
];

/// The plugin module. It reads `llmenv-hooks.json` from its own folder.
const PLUGIN_JS: &str = include_str!("plugin.js");

/// The longest that one user hook may run, in milliseconds, as the 1.x shim allowed.
const USER_HOOK_TIMEOUT_MS: u64 = 30_000;

/// The longest that one built-in `llmenv` hook may run. Each one reads local state only.
const BUILTIN_HOOK_TIMEOUT_MS: u64 = 5_000;

/// The tools whose edits the config guard checks: the Claude Code file tools, and opencode's
/// `patch`, which keeps its own name and writes files through a patch text.
const CONFIG_GUARD_MATCHER: &str = "Write|Edit|MultiEdit|patch";

/// The neutral tools that `hook-run pre_tool_use` must see on opencode: the codebase-memory
/// `index_repository` guard (#1331), and `Bash` for the commit gate of the task tracker (#2456).
pub(super) fn pre_tool_hook_tools(manifest: &crate::merge::MergedManifest) -> Vec<&'static str> {
    let mut tools = Vec::new();
    if manifest
        .mcps
        .iter()
        .any(|m| m.name == crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME)
    {
        tools.push(crate::hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL);
    }
    let enforce_commit = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled && t.enforce_commit);
    if enforce_commit {
        tools.push("Bash");
    }
    tools
}

/// Write the plugin and its hook table into `out/plugin`, and return the owned paths.
///
/// `pre_tools` are the neutral tool names that get `hook-run pre_tool_use`: the
/// `index_repository` name guard (#1331) and the commit gate (#2456). A hook that runs on every
/// tool call costs each call, so the entry names these tools only, and none means no entry.
/// opencode 2.x has no todo tool, so the tracker has nothing to redirect there.
pub(super) fn write(
    out: &Path,
    hooks: &[crate::config::Hook],
    pre_tools: &[&str],
    mcp_servers: &[&str],
) -> anyhow::Result<Vec<PathBuf>> {
    use anyhow::Context as _;

    let table = hook_table(hooks, pre_tools, mcp_servers)?;
    let plugin_dir = out.join("plugin");
    std::fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("creating {}", plugin_dir.display()))?;
    for (name, bytes) in [
        ("llmenv.js", PLUGIN_JS.as_bytes().to_vec()),
        ("llmenv-hooks.json", serde_json::to_vec(&table)?),
    ] {
        let path = plugin_dir.join(name);
        crate::paths::write_owner_only(&path, &bytes)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(vec![
        PathBuf::from("plugin/llmenv.js"),
        PathBuf::from("plugin/llmenv-hooks.json"),
    ])
}

/// The hook table: the user hooks and the built-in hooks, the opencode tool map, and the MCP
/// namespaces. A hook keeps its matcher, so it runs only for the tools it names.
pub(super) fn hook_table(
    hooks: &[crate::config::Hook],
    pre_tools: &[&str],
    mcp_servers: &[&str],
) -> anyhow::Result<serde_json::Value> {
    let mut entries: Vec<serde_json::Value> = hooks.iter().filter_map(user_hook_entry).collect();
    let builtin = |event: &str, matcher: Option<&str>, command: &str| {
        serde_json::json!({
            "event": event,
            "matcher": matcher,
            "command": command,
            "timeout": BUILTIN_HOOK_TIMEOUT_MS,
        })
    };
    entries.push(builtin(
        "SessionStart",
        None,
        &hook_command("check-stale --engine opencode")?,
    ));
    entries.push(builtin(
        "SessionStart",
        None,
        &hook_command("config-context --engine opencode")?,
    ));
    entries.push(builtin(
        "PreToolUse",
        Some(CONFIG_GUARD_MATCHER),
        &hook_command("config-guard --engine opencode")?,
    ));
    if !pre_tools.is_empty() {
        // The commit gate and the index guard deny on purpose, so a run that times out or cannot
        // start blocks the call instead of letting it through.
        let mut gate = builtin(
            "PreToolUse",
            Some(&pre_tools.join("|")),
            &hook_command("hook-run pre_tool_use --engine opencode")?,
        );
        gate["timeout"] = USER_HOOK_TIMEOUT_MS.into();
        gate["failClosed"] = true.into();
        entries.push(gate);
    }
    let mut namespaces = serde_json::Map::new();
    for server in mcp_servers {
        let namespace = crate::adapter::tools::opencode_hooks::mcp_namespace(server);
        if let Some(other) = namespaces.insert(namespace.clone(), serde_json::Value::from(*server))
        {
            eprintln!(
                "warning: the MCP servers {other} and {server} share the opencode tool prefix \
                 `{namespace}_`, so a hook sees the tools of both as tools of {server}. Fix: \
                 rename one of the two servers"
            );
        }
    }
    Ok(serde_json::json!({
        "hooks": entries,
        "tools": crate::adapter::tools::opencode_hooks::tools_json(),
        "mcpServers": namespaces,
    }))
}

/// The table entry for one user hook, or `None` for a hook with no command. Such a hook (an
/// `mcp_tool` handler) cannot run from the plugin, so it is skipped with a warning.
fn user_hook_entry(hook: &crate::config::Hook) -> Option<serde_json::Value> {
    let Some(command) = hook.handler.command.as_deref() else {
        eprintln!(
            "warning: a {} hook has no command, so opencode skips it. Fix: give the hook a \
             `command`, or remove it from the opencode scope",
            hook.event
        );
        return None;
    };
    let command = match &hook.bundle_origin {
        // `None` means no word of the command is a bundle-relative path, so it runs as written.
        Some(bundle_dir) => super::super::resolve_bundle_relative_paths(command, bundle_dir)
            .unwrap_or_else(|| command.to_string()),
        None => command.to_string(),
    };
    Some(serde_json::json!({
        "event": hook.event,
        "matcher": hook.matcher,
        "command": command,
        "timeout": USER_HOOK_TIMEOUT_MS,
    }))
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test code")]
mod tests;
