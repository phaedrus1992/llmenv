//! Detaches the WebFetch/WebSearch ICM memory store call into a background
//! child process so a PostToolUse hook returns immediately instead of blocking
//! on the MCP network round trip. `handle_web_fetch_post_tool_use` in
//! `hook_run/mod.rs` is the parent-side launcher; `run_icm_store` is the child
//! entrypoint, wired to the hidden `llmenv icm-store` command.

use std::time::Duration;

use crate::hook_run::mcp_client::McpHttpClient;

/// Per-call network timeout for the detached child's ICM memory store call.
const STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Child entrypoint: parse the `{content, topic, importance}` stdin payload,
/// resolve the active memory backend the same way a hook process would, and
/// store the memory. The child's stdout/stderr are null-redirected by the parent
/// (`handle_web_fetch_post_tool_use`), so on error this also logs via
/// `tracing::warn!` — otherwise the failure would be invisible even with
/// `RUST_LOG=debug`, since there's no terminal to write to.
///
/// # Errors
/// Malformed payload, no active memory backend, an invalid backend URL, or
/// the MCP call itself failing.
pub fn run_icm_store(payload_json: &str) -> anyhow::Result<()> {
    run_icm_store_inner(payload_json).inspect_err(|e| {
        tracing::warn!("icm-store: detached store failed: {e}");
    })
}

fn run_icm_store_inner(payload_json: &str) -> anyhow::Result<()> {
    let args: serde_json::Value = serde_json::from_str(payload_json)?;

    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;
    let client = McpHttpClient::new(url, STORE_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.call_tool("icm_memory_store", args))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn run_icm_store_rejects_malformed_payload_json() {
        let err = run_icm_store("not json").unwrap_err();
        assert!(err.to_string().to_lowercase().contains("expected"));
    }
}
