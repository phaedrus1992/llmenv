//! TTL-based memory retention pruning (R4).
//!
//! `llmenv memory prune [--dry-run]` queries the ICM MCP backend for stored
//! memories, evaluates each against the active retention policy, and forgets
//! expired records.
//!
//! ## Importance, not age
//!
//! Prune reads the JSON recall reply (#3368). It uses **importance as a proxy**
//! for age: low- and medium-importance memories are pruned, while high and
//! critical memories are always kept. This never discards high-signal data.
//!
//! The JSON reply has `created_at`, but no memory type, so the per-type
//! durations of `memory.retention` cannot be applied. When `retention` is set,
//! prune refuses to run and forgets nothing (#2386). It never falls back to the
//! importance proxy for a user who configured durations. Age-based pruning is
//! #3420.

use std::time::Duration;

use crate::hook_run::mcp_client::McpHttpClient;

/// CLI timeout — longer than hook timeout since users are waiting.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

mod records;
mod report;
use records::{Importance, MemoryRecord, read_records};

/// Result of a single prune run.
#[derive(Debug, Default, PartialEq)]
pub struct PruneResult {
    /// Total records found.
    pub(crate) total: usize,
    /// Critical-importance records (always kept).
    critical: usize,
    /// High-importance records (always kept).
    high: usize,
    /// Medium-importance records (prune candidates).
    medium: usize,
    /// Low-importance records (prune candidates).
    low: usize,
    /// Records with an importance that prune does not know (always kept).
    unknown: usize,
    /// Records that the pass forgets, or would forget in a dry run.
    candidates: usize,
    /// Number of records actually forgotten (non-dry-run only).
    pub(crate) forgotten: usize,
    /// Whether this was a dry run (no forget calls made).
    dry_run: bool,
}

fn load_config() -> anyhow::Result<(std::path::PathBuf, crate::config::Config)> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    Ok((config_path, config))
}

/// Refuse to prune when the active memory entry sets `retention`: its per-type durations need
/// each record's age and type, which the ICM recall output does not carry. `memory` is the
/// merged list of top-level and bundle-declared entries, the list that picks the server.
fn ensure_retention_unset(
    memory: &[crate::config::Memory],
    active_tags: &std::collections::BTreeSet<String>,
) -> anyhow::Result<()> {
    let configured = super::auto_prune::single_active(memory, active_tags)
        .is_some_and(|m| m.retention.is_some());
    if configured {
        anyhow::bail!(
            "memory prune cannot apply `memory.retention`: ICM's recall output has no \
             created_at or memory type, so no record can be aged. Nothing was forgotten. \
             Remove `retention` from the config to use the importance-based prune, or \
             wait for ICM to expose timestamps."
        );
    }
    Ok(())
}

fn connect(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<McpHttpClient> {
    let url = crate::hook_run::memory_url(config, config_dir, active)?.into_url()?;
    McpHttpClient::new(url, CLI_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))
}

/// Bridge a synchronous CLI context to an async MCP tool call.
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

/// The recall arguments for the records to prune: the project's own, at most [`RECALL_LIMIT`].
/// Prune forgets what it reads, so a folder with no known project is an error: without a project
/// filter ICM would answer with the records of its own working folder.
///
/// # Errors
/// `project` is `None`.
fn recall_args(project: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let project = project.ok_or_else(|| {
        anyhow::anyhow!(
            "memory prune cannot tell the project of this folder, and it forgets what it reads. \
             Run it inside a git repository or a project folder. Nothing was forgotten."
        )
    })?;
    Ok(serde_json::json!({
        "query": "",
        "limit": RECALL_LIMIT,
        "project": project,
        "format": crate::memory::recall_json::FORMAT,
    }))
}

/// The most records one prune pass reads.
const RECALL_LIMIT: u32 = 100;

/// Forget each candidate, and give the count that ICM forgot. A failed forget is printed to
/// stderr and the rest still run.
fn forget_all(client: &McpHttpClient, candidates: &[&MemoryRecord]) -> usize {
    let mut forgotten = 0;
    for candidate in candidates {
        match call_tool_blocking(
            client.clone(),
            "icm_memory_forget",
            serde_json::json!({ "id": candidate.id }),
        ) {
            Ok(_) => forgotten += 1,
            Err(e) => eprintln!("  Failed to forget {}: {e}", candidate.id),
        }
    }
    forgotten
}

/// Run the prune pass: query ICM, evaluate candidates, forget if not dry-run.
///
/// `dry_run` when true prints what would be pruned without making forget
/// calls. Returns a [`PruneResult`] with counts.
pub(crate) fn run(dry_run: bool) -> anyhow::Result<PruneResult> {
    let (config_path, config) = load_config()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let active = crate::scope::evaluate(
        &config,
        &crate::scope::matcher::Env::detect_for_config(&config)?,
    );
    let merged = crate::hook_run::merged_memory(&config, config_dir, &active)?;
    let result = run_with(&config, config_dir, &active, &merged.memory, dry_run)?;
    report::print(&result);
    Ok(result)
}

/// [`run`] for scopes and memory entries that the caller already resolved, so the caller's
/// `auto_prune` check and the pruned backend come from the same entry (#2947). It prints
/// nothing to stdout: `llmenv export` runs it, and the shell evaluates that stdout.
///
/// # Errors
/// The active entry sets `retention`, the memory server cannot be reached, or a call fails.
pub(super) fn run_with(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    memory: &[crate::config::Memory],
    dry_run: bool,
) -> anyhow::Result<PruneResult> {
    ensure_retention_unset(memory, &active.tags)?;
    let client = connect(config, config_dir, active)?;
    let cwd = std::env::current_dir().map_err(|e| {
        anyhow::anyhow!("cannot read the working directory to name the project: {e}")
    })?;
    let project = super::project::session_project(&cwd)?;
    let output = call_tool_blocking(
        client.clone(),
        "icm_memory_recall",
        recall_args(Some(project.as_str()))?,
    )?;

    let records = read_records(&output).map_err(|e| {
        anyhow::anyhow!("memory prune cannot read the recall reply: {e}. Nothing was forgotten.")
    })?;
    let mut result = PruneResult {
        total: records.len(),
        dry_run,
        ..Default::default()
    };
    for rec in &records {
        match rec.importance {
            Importance::Critical => result.critical += 1,
            Importance::High => result.high += 1,
            Importance::Medium => result.medium += 1,
            Importance::Low => result.low += 1,
            Importance::Unknown => result.unknown += 1,
        }
    }
    let candidates: Vec<&MemoryRecord> = records
        .iter()
        .filter(|r| r.importance.should_prune())
        .collect();
    result.candidates = candidates.len();
    if !dry_run {
        result.forgotten = forget_all(&client, &candidates);
    }
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config_with(memory_yaml: &str) -> crate::config::Config {
        serde_yaml::from_str(&format!("features:\n  memory:\n    - {memory_yaml}\n")).unwrap()
    }

    fn tags(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn prune_reads_one_project_and_refuses_when_it_cannot_tell_which() {
        let args = recall_args(Some("llmenv")).unwrap();
        assert_eq!(args["project"], "llmenv");
        assert_eq!(args["limit"], 100);
        assert_eq!(args["format"], "json");
        let err = recall_args(None).unwrap_err().to_string();
        assert!(err.contains("Nothing was forgotten"), "{err}");
    }

    #[test]
    fn configured_retention_blocks_prune() {
        let config = config_with("{server_host: h, port: 1, when: [t], retention: {episodic: 1d}}");
        let memory = &config.features.as_ref().unwrap().memory;
        let err = ensure_retention_unset(memory, &tags(&["t"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Nothing was forgotten"), "{err}");
    }

    #[test]
    fn retention_on_an_inactive_entry_does_not_block_prune() {
        let config = config_with("{server_host: h, port: 1, when: [t], retention: {episodic: 1d}}");
        let memory = &config.features.as_ref().unwrap().memory;
        ensure_retention_unset(memory, &tags(&["other"])).unwrap();
    }

    #[test]
    fn absent_retention_allows_prune() {
        let config = config_with("{server_host: h, port: 1, when: [t]}");
        let memory = &config.features.as_ref().unwrap().memory;
        ensure_retention_unset(memory, &tags(&["t"])).unwrap();
        ensure_retention_unset(&[], &tags(&["t"])).unwrap();
    }

    fn rec(id: &str, importance: &str) -> String {
        format!(
            r#"{{"id":"{id}","topic":"t","summary":"s","importance":"{importance}","weight":0.5}}"#
        )
    }

    #[test]
    fn read_records_maps_each_importance() {
        let body = format!(
            "[{},{},{},{},{}]",
            rec("a", "critical"),
            rec("b", "high"),
            rec("c", "medium"),
            rec("d", "low"),
            rec("e", "Critical")
        );
        let got: Vec<(String, Importance)> = read_records(&body)
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.importance))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".into(), Importance::Critical),
                ("b".into(), Importance::High),
                ("c".into(), Importance::Medium),
                ("d".into(), Importance::Low),
                ("e".into(), Importance::Unknown),
            ]
        );
        assert!(!Importance::Unknown.should_prune());
    }

    #[test]
    fn read_records_refuses_a_text_reply_so_nothing_is_forgotten() {
        let text = "--- id-1 ---\n  importance: low\n  weight: 0.1\n";
        let err = read_records(text).unwrap_err().to_string();
        assert!(err.contains("Fix: upgrade icm to 0.11.5 or later"), "{err}");
    }

    #[test]
    fn read_records_refuses_the_whole_reply_when_one_record_is_bad() {
        let body = format!(r#"[{},{{"topic":"t"}}]"#, rec("a", "low"));
        assert!(read_records(&body).is_err());
    }

    #[test]
    fn read_records_reads_an_empty_reply_as_no_records() {
        assert!(read_records("[]").unwrap().is_empty());
    }

    #[test]
    fn importance_should_prune() {
        assert!(!Importance::Critical.should_prune());
        assert!(!Importance::High.should_prune());
        assert!(Importance::Medium.should_prune());
        assert!(Importance::Low.should_prune());
    }
}
