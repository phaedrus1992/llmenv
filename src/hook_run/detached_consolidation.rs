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
pub fn run_consolidation(config_path: &std::path::Path) -> anyhow::Result<()> {
    run_consolidation_at(config_path).inspect_err(|e| {
        tracing::error!("consolidation-run: detached consolidation failed: {e:#}");
    })
}

// Real config load, scope detection, and a live MCP network call end to end
// — there is no seam here to inject a fake config/client without a larger
// dependency-injection refactor, so mutation testing (which would otherwise
// flag a mutant that replaces this whole body with `Ok(())`) cannot say
// anything useful about it. The tests below still exercise the real function
// through `run_consolidation` and assert the no-panic invariant.
#[mutants::skip]
fn run_consolidation_at(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = crate::config::Config::load(config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config);
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    // The merged list, so a bundle-declared memory entry's consolidation
    // settings count (#2355), the same as its endpoint does.
    let merged = crate::memory::merged_memory(&config, config_dir, &active)?;
    let Some(cc) = consolidation::active_consolidation(&merged.memory, &active.tags) else {
        return Ok(());
    };
    let url = crate::memory::memory_url(&config, config_dir, &active)?.into_url()?;
    let client = McpHttpClient::new(url, CONSOLIDATION_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    // The parent starts this child in the session's directory, the same one the
    // scope detection above reads.
    let cwd = std::env::current_dir().map_err(|e| {
        anyhow::anyhow!("cannot read the working directory to name the project: {e}")
    })?;
    let Some(project) = crate::memory::project::session_project(&cwd) else {
        tracing::error!(
            "consolidation-run: no project name for {}; consolidation skipped",
            cwd.display()
        );
        return Ok(());
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _result = rt.block_on(consolidation::run(cc, &client, &project))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn run_consolidation_reports_a_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_consolidation(&dir.path().join("config.yaml")).unwrap_err();
        assert!(!format!("{err:#}").is_empty());
    }

    #[test]
    fn run_consolidation_is_a_no_op_without_enabled_consolidation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "adapter:\n  engine: claude-code\n").unwrap();
        run_consolidation(&path).unwrap();
    }
}
