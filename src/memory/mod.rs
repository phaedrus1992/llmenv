//! `llmenv memory` subcommand — inspect ICM memory state (R2).
//!
//! Commands:
//! - `stats`  — record counts by tag/bundle/type
//! - `list`   — list stored memories for the active scope
//! - `diff`   — show what changed since last session
//! - `prune`  — TTL-based memory forgetting (R4)

pub mod prune;

use std::path::Path;
use std::time::Duration;

use crate::hook_run::mcp_client::McpHttpClient;

/// CLI timeout — longer than hook timeout since users are waiting.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

fn connect() -> anyhow::Result<McpHttpClient> {
    connect_with_timeout(CLI_TIMEOUT)
}

fn connect_with_timeout(timeout: Duration) -> anyhow::Result<McpHttpClient> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;
    McpHttpClient::new(url, timeout).map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))
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
pub fn stats_json() -> anyhow::Result<String> {
    let client = connect()?;
    call_tool_blocking(client, "icm_memory_stats", serde_json::json!({}))
}

/// Same as `stats_json` but with an injectable timeout, for background
/// collectors (the statusline data collector, #836) that must not stall a
/// hot path — materialization, `llmenv export`, or session start — waiting
/// on a slow/unreachable ICM backend. Returns `Err` when no memory backend
/// is active for the current scope or the MCP call fails/times out —
/// callers treat that as "no ICM stats available", not a hard error.
pub fn stats_json_with_timeout(timeout: Duration) -> anyhow::Result<String> {
    let client = connect_with_timeout(timeout)?;
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

/// Read the previous snapshot, or write `current` as the baseline on first run.
///
/// Returns `None` when there was no prior snapshot (baseline just written),
/// `Some(previous)` otherwise.
///
/// #911: a single read that maps `NotFound` → first-run baseline init and
/// propagates every other I/O error, rather than an `exists()` stat that masked
/// a permission error as "no snapshot" — which would then overwrite an existing
/// baseline with the current state and silently skip the diff.
fn read_or_init_snapshot(snapshot_path: &Path, current: &str) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(snapshot_path) {
        Ok(previous) => Ok(Some(previous)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::paths::write_owner_only_atomic(snapshot_path, current.as_bytes())?;
            Ok(None)
        }
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("reading {}", snapshot_path.display())))
        }
    }
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

    let Some(previous) = read_or_init_snapshot(&snapshot_path, &current)? else {
        println!("No previous snapshot to diff against. Saved current state as baseline.");
        return Ok(());
    };
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::read_or_init_snapshot;

    #[test]
    fn first_run_writes_baseline_and_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot");
        let got = read_or_init_snapshot(&path, "current state").unwrap();
        assert_eq!(got, None);
        // Baseline was written with the current state.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "current state");
    }

    #[test]
    fn returns_previous_without_clobbering_when_snapshot_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot");
        std::fs::write(&path, "previous state").unwrap();
        let got = read_or_init_snapshot(&path, "current state").unwrap();
        assert_eq!(got.as_deref(), Some("previous state"));
        // The existing baseline was not overwritten with the current state.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous state");
    }

    // #911: a non-NotFound I/O error (EACCES) must propagate, not be masked as
    // "no snapshot" — which would clobber the baseline.
    #[cfg(unix)]
    #[test]
    fn propagates_permission_error_instead_of_masking_absent() {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("snapshot");
        fs::write(&path, "previous state").unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o000)).unwrap();
        let result = read_or_init_snapshot(&path, "current state");
        let readable_anyway = fs::read_dir(&dir).is_ok();
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap(); // restore for cleanup
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "permission error must propagate, got {result:?}"
        );
    }
}
