//! Detaches post-session memory consolidation into a background child process so
//! the SessionEnd/PostSession hook returns immediately instead of blocking on MCP
//! round trips. Consolidation is fire-and-forget — the result text is not captured
//! for adapter context (PostSession is the final event).

use std::time::Duration;

use crate::consolidation;
use crate::hook_run::mcp_client::McpHttpClient;

/// Per-call network timeout for the detached child's consolidation MCP calls.
const CONSOLIDATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Child entrypoint: load config from disk, resolve the active memory backend
/// the same way a hook process would, and run post-session consolidation. The
/// parent points this child's stderr at a bounded log, and errors log via
/// `tracing::error!` rather than `warn!` because the default `EnvFilter`
/// (`RUST_LOG` unset) is ERROR-only and dropped the warning before it could
/// reach that log (#1133).
///
/// With a `checkpoint` (#2396) the run records its phase there. The file stays after a failure
/// that a later run may fix, and goes away when nothing is left to do.
///
/// # Errors
/// Malformed or missing config, no active memory backend, invalid backend URL,
/// or an MCP call failure.
pub fn run_consolidation(
    config_path: &std::path::Path,
    checkpoint: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    run_consolidation_at(config_path, checkpoint).inspect_err(|e| {
        tracing::error!("consolidation-run: detached consolidation failed: {e:#}");
    })
}

/// Delete the checkpoint of a run that has nothing to do, such as consolidation switched off.
fn settle(checkpoint: Option<&std::path::Path>) {
    if let Some(path) = checkpoint
        && let Err(e) = crate::hook_run::checkpoint::complete(path)
    {
        tracing::error!("consolidation-run: {e:#}");
    }
}

fn run_consolidation_at(
    config_path: &std::path::Path,
    checkpoint: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let config = crate::config::Config::load(config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config);
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    // The merged list, so a bundle-declared memory entry's consolidation
    // settings count (#2355), the same as its endpoint does.
    let merged = crate::hook_run::merged_memory(&config, config_dir, &active)?;
    let Some(cc) = consolidation::active_consolidation(&merged.memory, &active.tags) else {
        settle(checkpoint);
        return Ok(());
    };
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?.into_url()?;
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
        settle(checkpoint);
        return Ok(());
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let state_dir = crate::paths::state_dir().ok();
    let _result = rt.block_on(consolidation::run(
        cc,
        &client,
        &project,
        state_dir.as_deref(),
        checkpoint,
    ))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn run_consolidation_reports_a_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_consolidation(&dir.path().join("config.yaml"), None).unwrap_err();
        assert!(!format!("{err:#}").is_empty());
    }

    #[test]
    fn run_consolidation_is_a_no_op_without_enabled_consolidation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "adapter:\n  engine: claude-code\n").unwrap();
        run_consolidation(&path, None).unwrap();
    }

    #[test]
    fn a_run_with_consolidation_off_deletes_its_checkpoint() {
        use crate::hook_run::checkpoint::{self, Checkpoint, JobKind};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "adapter:\n  engine: claude-code\n").unwrap();
        let cp = Checkpoint::new(JobKind::Consolidation, serde_json::json!({}), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        run_consolidation(&path, Some(&file)).unwrap();
        assert!(!file.exists(), "nothing left to do, so the checkpoint goes");
    }

    #[test]
    fn a_failed_run_keeps_its_checkpoint() {
        use crate::hook_run::checkpoint::{self, Checkpoint, JobKind};
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(JobKind::Consolidation, serde_json::json!({}), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        run_consolidation(&dir.path().join("missing.yaml"), Some(&file)).unwrap_err();
        assert!(
            file.exists(),
            "a failure a later run may fix keeps the file"
        );
    }
}
