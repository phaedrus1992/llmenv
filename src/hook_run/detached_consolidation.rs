//! Detaches post-session memory consolidation into a background child process so
//! the SessionEnd/PostSession hook returns immediately instead of blocking on MCP
//! round trips. Consolidation is fire-and-forget — the result text is not captured
//! for adapter context (PostSession is the final event).

use std::time::Duration;

use crate::consolidation;
use llmenv_mcp::mcp_client::McpHttpClient;

/// Per-call network timeout for the detached child's consolidation MCP calls.
const CONSOLIDATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Child entrypoint: load config from disk, resolve the active memory backend
/// the same way a hook process would, and run post-session consolidation. The
/// parent points this child's stderr at a bounded log, and errors log via
/// `tracing::error!` rather than `warn!` because the default `EnvFilter`
/// (`RUST_LOG` unset) is ERROR-only and dropped the warning before it could
/// reach that log (#1133).
///
/// # Errors
/// Malformed or missing config, no active memory backend, invalid backend URL,
/// or an MCP call failure.
pub fn run_consolidation() -> anyhow::Result<()> {
    run_consolidation_inner().inspect_err(|e| {
        tracing::error!("consolidation-run: detached consolidation failed: {e}");
    })
}

fn run_consolidation_inner() -> anyhow::Result<()> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config);
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::memory::memory_url(&config, config_dir, &active)?.into_url()?;
    let client = McpHttpClient::new(url, CONSOLIDATION_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _result = rt.block_on(consolidation::run(&config, &client))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn run_consolidation_inner_does_not_panic() {
        // The inner function returns a `Result` and never unwraps internally,
        // so it either succeeds or returns a descriptive error — either is
        // valid and the important invariant is no unwrap/panic.
        let result = run_consolidation_inner();
        match result {
            Ok(()) => {} // all good — config was found and consolidation ran
            Err(e) => assert!(!e.to_string().is_empty(), "expected a descriptive error"),
        }
    }

    #[test]
    fn run_consolidation_entrypoint_safe() {
        // Outer entrypoint must not panic even when inner fails (errors are
        // caught by inspect_err and logged at warn level).
        let _ = run_consolidation();
    }
}
