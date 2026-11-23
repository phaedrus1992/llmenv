//! Renders the Crush `mcp` object from llmenv's MCP servers.

use serde_json::json;

/// Build the `mcp` object, keyed by server name, with `native_mcp.crush` overlaid last.
///
/// Crush's `MCPConfig.Type` is required and takes exactly `stdio`, `sse`, or `http`. Its MCP
/// client returns "unsupported mcp type" for anything else, so every server carries one of
/// those values. Returns `None` when no server remains, so no empty key is written.
pub(super) fn render_mcp(
    manifest: &crate::merge::MergedManifest,
) -> anyhow::Result<Option<serde_json::Value>> {
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
                    e.insert("env".into(), super::render::render_env(env));
                }
                e
            }
            ResolvedKind::Remote { url, transport } => {
                let mut e = serde_json::Map::new();
                e.insert(
                    "type".into(),
                    json!(crate::adapter::remote_transport_type_str(*transport)),
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
    let mut mcp_value = serde_json::Value::Object(mcp_obj);
    crate::adapter::overlay_native_json(
        &mut mcp_value,
        manifest.capabilities.native_mcp.get("crush"),
        "native_mcp.crush",
    )?;
    Ok((!mcp_value.as_object().is_none_or(serde_json::Map::is_empty)).then_some(mcp_value))
}

#[cfg(test)]
mod tests {
    use super::render_mcp;
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::merge::MergedManifest;

    #[test]
    fn mcp_stdio_env_values_are_escaped_for_crush() -> anyhow::Result<()> {
        let mut env = std::collections::BTreeMap::new();
        env.insert("CMD".to_string(), "$(id)".to_string());
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(ResolvedMcp {
            name: "srv".to_string(),
            kind: ResolvedKind::Stdio {
                command: "srv-bin".to_string(),
                args: vec![],
                env,
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
            always_load: None,
        });
        let rendered = render_mcp(&manifest)?.ok_or_else(|| anyhow::anyhow!("no mcp object"))?;
        assert_eq!(rendered["srv"]["env"]["CMD"], "\\$(id)");
        Ok(())
    }
}
