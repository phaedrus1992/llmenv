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
/// store the memory. There's no terminal to write to, so on error this logs via
/// `tracing::error!` and the parent (`handle_web_fetch_post_tool_use`) points
/// the child's stderr at a bounded log — `error!` rather than `warn!` because
/// the default `EnvFilter` (`RUST_LOG` unset) is ERROR-only and dropped the
/// warning before it could reach that log (#1133).
///
/// # Errors
/// Malformed payload, no active memory backend, an invalid backend URL, or
/// the MCP call itself failing.
pub fn run_icm_store(payload_json: &str) -> anyhow::Result<()> {
    run_icm_store_inner(payload_json).inspect_err(|e| {
        tracing::error!("icm-store: detached store failed: {e}");
    })
}

fn run_icm_store_inner(payload_json: &str) -> anyhow::Result<()> {
    let args: serde_json::Value = serde_json::from_str(payload_json)?;

    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config);
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?.into_url()?;
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

    // #1133: this child's only report channel is its (now log-redirected)
    // stderr, and the default `EnvFilter` with `RUST_LOG` unset is ERROR-only —
    // a `warn!` here was dropped before it could reach that log.
    //
    // The malformed-payload rejection is asserted in the same test on purpose:
    // `tracing` caches a callsite's interest globally on first hit, so a
    // sibling test reaching this `error!` outside any subscriber would make the
    // capture order-dependent.
    #[test]
    fn run_icm_store_rejects_malformed_payload_json_and_logs_at_error_level() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.jsonl");
        let err = crate::session_log::tracing_layer::capture_logs_at(
            &log,
            tracing_subscriber::filter::LevelFilter::ERROR,
            || run_icm_store("not json").unwrap_err(),
        );

        assert!(err.to_string().to_lowercase().contains("expected"));
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            body.contains("detached store failed"),
            "the failure must log at a level the default EnvFilter passes: {body}"
        );
    }
}
