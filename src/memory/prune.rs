//! TTL-based memory retention pruning (R4).
//!
//! `llmenv memory prune [--dry-run]` queries the ICM MCP backend for stored
//! memories, evaluates each against the active retention policy, and forgets
//! expired records.
//!
//! ## Limitation: no creation timestamps in MCP output
//!
//! The ICM MCP `format_memory_output()` (the text representation returned by
//! `icm_memory_recall`) does not include `created_at` / `updated_at` fields.
//! The ICM [`Memory`] struct has these, but the MCP formatting function
//! omits them. Without per-memory timestamps the prune cannot implement true
//! age-based TTL filtering, so it uses **importance as a proxy**: low- and
//! medium-importance memories are pruned by default, while high/critical
//! memories are always preserved. This is conservative (never discards high-
//! signal data) and mirrors the intent of an age-based policy (recent
//! important data → kept, old noise → removed) without needing absolute
//! dates.
//!
//!   ponytail: importance-proxy TTL. Add true age-based pruning when ICM's
//!   MCP output includes `created_at` timestamps (icm upstream issue #?).

use std::time::Duration;

use crate::hook_run::mcp_client::McpHttpClient;

/// CLI timeout — longer than hook timeout since users are waiting.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

/// A parsed memory record from the non-compact `icm_memory_recall` output.
#[derive(Debug, Clone)]
struct MemoryRecord {
    id: String,
    importance: Importance,
}

/// Parsed importance levels from the MCP output.
#[derive(Debug, Clone, PartialEq)]
enum Importance {
    Critical,
    High,
    Medium,
    Low,
}

impl Importance {
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "critical" => Self::Critical,
            "high" => Self::High,
            "medium" => Self::Medium,
            _ => Self::Low,
        }
    }

    fn should_prune(&self) -> bool {
        matches!(self, Self::Medium | Self::Low)
    }
}

/// Result of a single prune run.
#[derive(Debug, Default, PartialEq)]
pub struct PruneResult {
    /// Total records found.
    pub total: usize,
    /// Critical-importance records (always kept).
    pub critical: usize,
    /// High-importance records (always kept).
    pub high: usize,
    /// Medium-importance records (prune candidates).
    pub medium: usize,
    /// Low-importance records (prune candidates).
    pub low: usize,
    /// Number of records actually forgotten (non-dry-run only).
    pub forgotten: usize,
    /// Whether this was a dry run (no forget calls made).
    pub dry_run: bool,
}

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

/// Parse the non-compact `icm_memory_recall` output into structured records.
///
/// The format (per ICM's `format_memory_output` with compact=false):
///
/// ```text
/// --- <id> ---
///   topic: ...
///   importance: ...
///   weight: ...
///   summary: ...
///   keywords: ...
///   raw: ...
///   score: ...
/// ```
fn parse_recall_output(text: &str) -> Vec<MemoryRecord> {
    let mut records = Vec::new();
    let mut current_id: Option<String> = None;
    let mut current_importance: Option<Importance> = None;
    // `has_weight` guards against partial records: we only finalize a record
    // once both importance and weight have been seen.
    let mut has_weight = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("--- ") {
            // new record start: "--- <id> ---"
            if let Some(id) = rest.strip_suffix(" ---") {
                // Finalize previous record
                if let (Some(id), Some(importance)) = (current_id.take(), current_importance.take())
                    && has_weight
                {
                    records.push(MemoryRecord { id, importance });
                }
                has_weight = false;
                current_id = Some(id.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("importance:") {
            current_importance = Some(Importance::from_str(rest));
        } else if let Some(rest) = trimmed.strip_prefix("weight:") {
            has_weight = rest.trim().parse::<f64>().is_ok();
        }
    }

    // Finalize the last record
    if let (Some(id), Some(importance)) = (current_id, current_importance)
        && has_weight
    {
        records.push(MemoryRecord { id, importance });
    }

    records
}

/// Run the prune pass: query ICM, evaluate candidates, forget if not dry-run.
///
/// `dry_run` when true prints what would be pruned without making forget
/// calls. Returns a [`PruneResult`] with counts.
pub fn run(dry_run: bool) -> anyhow::Result<PruneResult> {
    let client = connect()?;
    let output = call_tool_blocking(
        client,
        "icm_memory_recall",
        serde_json::json!({ "query": "", "limit": 100 }),
    )?;

    let records = parse_recall_output(&output);
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
        }
    }

    // Identify prune candidates
    let candidates: Vec<&MemoryRecord> = records
        .iter()
        .filter(|r| r.importance.should_prune())
        .collect();

    if dry_run {
        println!("Prune dry-run — no changes made.");
        println!("  Total records:       {}", result.total);
        println!("  critical (kept):     {}", result.critical);
        println!("  high (kept):         {}", result.high);
        println!("  medium (candidate):  {}", result.medium);
        println!("  low (candidate):     {}", result.low);
        println!(
            "  Would prune:         {} record(s) (low + medium importance)",
            candidates.len()
        );
        if result.total > 0 {
            println!(
                "  Would keep:          {} record(s) (critical + high importance)",
                result.critical + result.high
            );
        }
        let pct = if result.total > 0 {
            candidates.len() as f64 / result.total as f64 * 100.0
        } else {
            0.0
        };
        if candidates.is_empty() {
            println!("  No prune candidates found.");
        } else {
            println!("  ({pct:.1}% of records are prune candidates)");
            println!();
            println!("  ponytail: age-based TTL unavailable because ICM's MCP output");
            println!("  omits `created_at` timestamps. This pass uses importance as a");
            println!("  heuristic: low and medium importance memories are candidates;");
            println!("  high and critical are always kept. Add date-based pruning once");
            println!("  ICM exposes timestamps through the MCP interface.");
        }
    } else {
        let forget_client = connect()?;
        let mut forgotten = 0;
        for candidate in &candidates {
            match call_tool_blocking(
                forget_client.clone(),
                "icm_memory_forget",
                serde_json::json!({ "id": candidate.id }),
            ) {
                Ok(_) => forgotten += 1,
                Err(e) => {
                    eprintln!("  Failed to forget {}: {e}", candidate.id);
                }
            }
        }
        result.forgotten = forgotten;

        println!("Prune complete.");
        println!("  Total records:       {}", result.total);
        println!("  critical (kept):     {}", result.critical);
        println!("  high (kept):         {}", result.high);
        println!("  medium (pruned):     {}", result.medium);
        println!("  low (pruned):        {}", result.low);
        println!("  Forgot:              {forgotten} record(s)");
        if forgotten != candidates.len() {
            println!(
                "  Skipped:             {} record(s) (forget errored)",
                candidates.len() - forgotten
            );
        }
    }

    Ok(result)
}

/// Run an automatic prune pass during materialize if the active config has
/// `auto_prune: true`. Fail-soft: errors are logged as warnings, never
/// returned.
pub fn auto_prune_if_enabled(config: &crate::config::Config) {
    let auto = config
        .features
        .as_ref()
        .and_then(|f| f.memory.iter().find(|m| m.auto_prune))
        .is_some();

    if !auto {
        return;
    }

    tracing::info!("auto_prune: running memory prune pass during materialize");
    match run(false) {
        Ok(result) => {
            tracing::info!(
                "auto_prune: forgot {} record(s) ({} total evaluated)",
                result.forgotten,
                result.total
            );
        }
        Err(e) => {
            tracing::warn!("auto_prune: prune pass failed (fail-soft): {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_recall_output_empty() {
        assert!(parse_recall_output("").is_empty());
    }

    #[test]
    fn parse_recall_output_single_record() {
        let text = "--- abc123 ---\n  topic: test\n  importance: high\n  weight: 0.85\n  summary: a test\n  keywords: test,example\n  raw:\n  score: 0.9\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "abc123");
        assert_eq!(records[0].importance, Importance::High);
    }

    #[test]
    fn parse_recall_output_multiple_records() {
        let text = "--- id-1 ---\n  importance: low\n  weight: 0.1\n  summary: first\n--- id-2 ---\n  importance: critical\n  weight: 0.99\n  summary: second\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, "id-1");
        assert_eq!(records[0].importance, Importance::Low);
        assert_eq!(records[1].id, "id-2");
        assert_eq!(records[1].importance, Importance::Critical);
    }

    #[test]
    fn importance_should_prune() {
        assert!(!Importance::Critical.should_prune());
        assert!(!Importance::High.should_prune());
        assert!(Importance::Medium.should_prune());
        assert!(Importance::Low.should_prune());
    }

    #[test]
    fn parse_recall_output_partial_record_omitted() {
        // A record without weight should be skipped (missing required fields)
        let text = "--- id-1 ---\n  importance: low\n  topic: test\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 0);
    }

    #[test]
    fn parse_recall_output_unparseable_weight_omitted() {
        let text = "--- id-1 ---\n  importance: low\n  weight: not-a-number\n  topic: test\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 0);
    }
}
