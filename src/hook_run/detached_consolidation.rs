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
/// Emits a `[LLMENV_DETACHED] consolidation` trace line with wall time, CPU time,
/// and exit status when `LLMENV_TRACE_TIMING` is set (#3014).
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
    let t0 = std::time::Instant::now();
    let result = run_consolidation_at(config_path, checkpoint);
    crate::trace::emit_detached_trace(&crate::trace::DetachedJob {
        kind: "consolidation",
        request_id: None,
        started: t0,
        exit: i32::from(result.is_err()),
    });
    result.inspect_err(|e| {
        // The child runs alone, so the notice reaches the agent through the spool (#3398).
        crate::agent_notice::record_logged(format!(
            "llmenv: memory consolidation failed: {e:#}. Fix: run `llmenv doctor` to check the \
             memory server and the consolidation backend."
        ));
    })
}

/// Delete the checkpoint of a run that has nothing to do, such as consolidation switched off.
/// A checkpoint that stays is a notice, because a later session runs the job again (#3465).
fn settle(checkpoint: Option<&std::path::Path>) {
    crate::hook_run::checkpoint::finish(checkpoint);
}

fn run_consolidation_at(
    config_path: &std::path::Path,
    checkpoint: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let config = crate::config::Config::load(config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
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
    // A lookup that fails leaves the checkpoint unsettled, so the next session runs it again.
    // The checkpoint stays unsettled on this error, so the next session retries the run.
    let project = crate::memory::project::session_project(&cwd).map_err(|e| {
        tracing::error!(
            "consolidation-run: cannot name the project of {}: {e:#}",
            cwd.display()
        );
        anyhow::anyhow!("consolidation-run: cannot name the project: {e:#}")
    })?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let state_dir = crate::paths::state_dir()
        .inspect_err(|e| {
            crate::agent_notice::record_logged(format!(
                "llmenv: memory consolidation has no state folder, so it skips the duplicate \
                 check and a rule can be stored twice: {e:#}. Fix: set HOME, or set \
                 LLMENV_STATE_DIR to a valid absolute path."
            ));
        })
        .ok();
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
    fn a_settled_checkpoint_that_stays_is_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let stuck = dir.path().join("consolidation-3465");
        std::fs::create_dir_all(stuck.join("inner")).unwrap();
        settle(Some(&stuck));
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("its checkpoint stays"), "{notices}");
    }

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
