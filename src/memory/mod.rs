//! `llmenv memory` subcommand — inspect ICM memory state (R2).
//!
//! Commands:
//! - `stats`  — record counts by tag/bundle/type
//! - `list`   — list stored memories for the active scope
//! - `diff`   — show what changed since last session
//! - `prune`  — TTL-based memory forgetting (R4)

pub mod prune;

use std::time::Duration;

use crate::hook_run::mcp_client::McpHttpClient;

/// CLI timeout — longer than hook timeout since users are waiting.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

fn connect() -> anyhow::Result<McpHttpClient> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;
    McpHttpClient::new(url, CLI_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))
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
/// for programmatic callers (the statusline data collector, #836). Returns
/// `Err` when no memory backend is active for the current scope or the MCP
/// call fails — callers treat that as "no ICM stats available", not a hard
/// error.
pub fn stats_json() -> anyhow::Result<String> {
    let client = connect()?;
    call_tool_blocking(client, "icm_memory_stats", serde_json::json!({}))
}

/// Run the `stats` subcommand: connect to ICM and output memory stats.
pub fn stats() -> anyhow::Result<()> {
    println!("{}", stats_json()?);
    Ok(())
}

/// Run the `list` subcommand: list stored memories for the active scope.
pub fn list() -> anyhow::Result<()> {
    let client = connect()?;
    let result = call_tool_blocking(
        client,
        "icm_memory_recall",
        serde_json::json!({ "query": "" }),
    )?;
    println!("{result}");
    Ok(())
}

/// Run the `diff` subcommand: compare current state with last snapshot.
pub fn diff() -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    let snapshot_path = state_dir.join(crate::paths::HOOK_STORE_CHUNK);

    let current = {
        let client = connect()?;
        call_tool_blocking(
            client,
            "icm_memory_recall",
            serde_json::json!({ "query": "" }),
        )?
    };

    if !snapshot_path.exists() {
        // First run: save current state as baseline.
        crate::paths::write_owner_only_atomic(&snapshot_path, current.as_bytes())?;
        println!("No previous snapshot to diff against. Saved current state as baseline.");
        return Ok(());
    }

    let previous = std::fs::read_to_string(&snapshot_path)?;
    if previous == current {
        println!("No changes since last snapshot.");
    } else {
        println!("Memory state has changed since last snapshot.");
        println!();
        println!("--- previous");
        println!("+++ current");
        for (prev, curr) in previous.lines().zip(current.lines()) {
            if prev != curr {
                println!("-{prev}");
                println!("+{curr}");
            }
        }
        // Extra lines in current
        for line in current.lines().skip(previous.lines().count()) {
            println!("+{line}");
        }
        // Extra lines in previous
        for line in previous.lines().skip(current.lines().count()) {
            println!("-{line}");
        }
    }

    // Update snapshot
    crate::paths::write_owner_only_atomic(&snapshot_path, current.as_bytes())?;
    Ok(())
}

/// Run the `prune` subcommand: evaluate and forget expired memories.
pub fn prune(dry_run: bool) -> anyhow::Result<()> {
    let result = prune::run(dry_run)?;
    if result.forgotten > 0 {
        tracing::info!("memory prune: forgot {} record(s)", result.forgotten);
    }
    Ok(())
}
