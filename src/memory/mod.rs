//! `llmenv memory` subcommand — inspect ICM memory state (R2).
//!
//! Commands:
//! - `stats`  — record counts by tag/bundle/type
//! - `list`   — list stored memories for the active scope
//! - `diff`   — show what changed since last session
//! - `prune`  — TTL-based memory forgetting (R4)

pub(crate) mod auto_prune;
mod listing;
pub(crate) mod project;
pub mod prune;
pub(crate) mod recall_json;
pub mod resolve;

use std::time::Duration;

use llmenv_mcp::mcp_client::McpHttpClient;

pub(crate) use resolve::{MemoryEndpoint, memory_url, merged_memory, suppressed_memory_bundles};

/// CLI timeout — longer than hook timeout since users are waiting.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

fn connect() -> anyhow::Result<McpHttpClient> {
    connect_with_timeout(CLI_TIMEOUT)
}

/// The scope has no memory backend. Most sessions have none, so a stats poll treats it as
/// absent and does not warn (#3062).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct NoBackendForScope(String);

/// True when `err` says the scope has no memory backend, not that a backend failed.
pub(crate) fn is_no_backend(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NoBackendForScope>().is_some()
}

fn connect_with_timeout(timeout: Duration) -> anyhow::Result<McpHttpClient> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
    let active = crate::scope::evaluate(&config, &env);
    // `into_url` fails only for an inactive endpoint, so every error here is "no backend".
    let url = memory_url(&config, config_dir, &active)?
        .into_url()
        .map_err(|e| NoBackendForScope(format!("{e:#}")))?;
    McpHttpClient::new(url, timeout)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e:#}"))
}

/// Bridge a synchronous CLI context to an async MCP tool call.
/// Creates a single-threaded tokio runtime inside `thread::scope` to run the
/// async call, then returns the result.
fn call_tool_blocking(
    client: McpHttpClient,
    tool: &str,
    args: serde_json::Value,
) -> anyhow::Result<String> {
    std::thread::scope(|s| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        s.spawn(move || rt.block_on(client.call_tool(tool, args)))
            .join()
            .map_err(|_| anyhow::anyhow!("call_tool_blocking thread panicked"))?
    })
}

/// Same as `stats()` but returns the raw JSON string instead of printing it,
/// for programmatic callers. Uses `CLI_TIMEOUT` — for an interactive command
/// where the user is watching and waiting is acceptable.
fn stats_json() -> anyhow::Result<String> {
    let client = connect()?;
    call_tool_blocking(client, "icm_memory_stats", serde_json::json!({}))
}

/// Same as `stats_json` but with an injectable timeout, for background
/// collectors (the statusline data collector, #836) that must not stall a
/// hot path — materialization, `llmenv export`, or session start — waiting
/// on a slow/unreachable ICM backend. Returns `Err` when no memory backend
/// is active for the current scope or the MCP call fails/times out —
/// callers treat that as "no ICM stats available", not a hard error.
pub(crate) fn stats_json_with_timeout(timeout: Duration) -> anyhow::Result<String> {
    let client = connect_with_timeout(timeout)?;
    call_tool_blocking(client, "icm_memory_stats", serde_json::json!({}))
}

/// Run the `stats` subcommand: connect to ICM and output memory stats.
pub(crate) fn stats() -> anyhow::Result<()> {
    println!("{}", stats_json()?);
    Ok(())
}

/// The `icm_memory_recall` arguments for the project that `cwd` belongs to.
/// ICM's default filter is the ICM server's cwd, which is unrelated to this
/// process when ICM runs on another host (#2253). No cwd, passed as `None`,
/// searches all projects on purpose.
///
/// # Errors
/// The project of `cwd` cannot be named. An unscoped recall would return other projects'
/// memories, so the command stops instead (#2688).
pub(crate) fn scope_recall_args(
    cwd: Option<&std::path::Path>,
) -> anyhow::Result<serde_json::Value> {
    let Some(dir) = cwd else {
        return Ok(serde_json::json!({ "query": "", "project": "" }));
    };
    let project = project::session_project(dir)?;
    Ok(serde_json::json!({ "query": "", "project": project }))
}

/// The project's records, read from the JSON form of recall (#3421).
fn scope_records() -> anyhow::Result<Vec<recall_json::RecallRecord>> {
    let client = connect()?;
    let cwd = std::env::current_dir()?;
    let args = listing::listing_args(scope_recall_args(Some(&cwd))?);
    let body = call_tool_blocking(client, "icm_memory_recall", args)?;
    recall_json::parse(&body).map_err(|e| anyhow::anyhow!("icm memory recall: {e}"))
}

/// Run the `list` subcommand: list stored memories for the active scope.
pub(crate) fn list() -> anyhow::Result<()> {
    println!("{}", listing::render_list(&scope_records()?));
    Ok(())
}

/// Run the `diff` subcommand: compare the project's records with the last `diff` run, by id.
pub(crate) fn diff() -> anyhow::Result<()> {
    let snapshot_path = crate::paths::state_dir()?.join(listing::DIFF_SNAPSHOT);
    let records = scope_records()?;
    // A recall at the limit holds only the first records. Saving it as the baseline would show the
    // rest as new on the next run, so the baseline is kept and no diff is shown (#3508).
    if let Some(note) = recall_json::read_limit_note(records.len()) {
        println!("{note} The baseline is not changed, so no diff is shown.");
        return Ok(());
    }
    let current: Vec<listing::SnapshotRecord> =
        records.iter().map(listing::SnapshotRecord::from).collect();
    let Some(previous) = listing::read_snapshot(&snapshot_path)? else {
        listing::write_snapshot(&snapshot_path, &current)?;
        println!("No previous snapshot to diff against. Saved current state as baseline.");
        return Ok(());
    };
    let lines = listing::diff_lines(&previous, &current);
    if lines.is_empty() {
        println!("No changes since last snapshot.");
    } else {
        println!("Memory state has changed since last snapshot.");
        println!();
        for line in lines {
            println!("{line}");
        }
        println!();
        println!("{}", listing::count_note(current.len()));
    }
    listing::write_snapshot(&snapshot_path, &current)
}

/// Run the `prune` subcommand: evaluate and forget expired memories.
pub(crate) fn prune(dry_run: bool) -> anyhow::Result<()> {
    let result = prune::run(dry_run)?;
    if result.forgotten > 0 {
        tracing::info!("memory prune: forgot {} record(s)", result.forgotten);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{NoBackendForScope, is_no_backend, scope_recall_args};

    #[test]
    fn a_missing_backend_is_no_backend_and_a_failed_call_is_not() {
        let missing = anyhow::Error::new(NoBackendForScope("no backend".to_owned()));
        assert!(is_no_backend(&missing));
        assert!(!is_no_backend(&anyhow::anyhow!("MCP call timed out")));
    }

    // #2253 variant: without `project`, ICM filters by the ICM server's cwd,
    // not by the project that `llmenv memory list` runs in.
    #[test]
    fn scope_recall_args_name_the_cwd_project() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("some-project");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            scope_recall_args(Some(&dir)).ok(),
            Some(serde_json::json!({ "query": "", "project": "some-project" }))
        );
    }

    #[test]
    fn scope_recall_args_search_all_projects_without_a_cwd() {
        assert_eq!(
            scope_recall_args(None).ok(),
            Some(serde_json::json!({ "query": "", "project": "" }))
        );
    }

    #[test]
    fn scope_recall_args_refuses_a_folder_it_cannot_name() {
        let err = scope_recall_args(Some(std::path::Path::new("/"))).err();
        assert!(
            err.is_some_and(|e| format!("{e:#}").contains("cannot tell the project")),
            "the root folder must stop the recall, not search all projects"
        );
    }
}
