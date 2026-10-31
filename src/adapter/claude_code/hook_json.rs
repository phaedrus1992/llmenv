//! Builds the handler object of one hook in Claude Code's `settings.json`.

/// Builds the handler object for `hook`, with `resolved_command` as its `command`.
///
/// Null-valued keys are omitted, not written as `null`. A null key differs from an absent key
/// in JSON equality, so a re-render would see a changed hook and add a duplicate (#699, #720).
pub(super) fn hook_handler_value(
    hook: &crate::config::Hook,
    resolved_command: Option<String>,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(cmd) = resolved_command {
        m.insert("command".into(), serde_json::Value::String(cmd));
    }
    if let Some(ref tool) = hook.handler.tool {
        m.insert("tool".into(), serde_json::Value::String(tool.clone()));
    }
    m.insert(
        "type".into(),
        serde_json::Value::String(
            match hook.handler.kind {
                crate::config::HookHandlerKind::Command => "command",
                crate::config::HookHandlerKind::McpTool => "mcp_tool",
            }
            .into(),
        ),
    );
    serde_json::Value::Object(m)
}
