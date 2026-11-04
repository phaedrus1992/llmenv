//! Collects the fields written to `llmenv-status.json` (the statusline data
//! file, see `crate::cli::statusline::data`). All I/O here is best-effort: a
//! sub-collector that can't get its data (no ICM backend active, cache dir
//! not yet materialized, etc.) degrades to `None` (or a well-defined zero
//! value) rather than failing the whole collection — materialize/export must
//! never abort because a stat is unavailable.
//!
//! The JSON shape is defined once, by `crate::cli::statusline::data::StatusData`
//! (the reader side, Task 2 of this plan) — this module reuses those types
//! rather than declaring a second, parallel schema that could drift.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

mod cache;
mod icm;
mod tasks;
#[cfg(test)]
use crate::cli::statusline::data::SessionProgress;
use cache::collect_cache;
use icm::collect_icm;
#[cfg(test)]
use icm::{extract_stat_line, parse_icm_stats};
use tasks::collect_tasks;
#[cfg(test)]
use tasks::collect_tasks_from_state_dir;

use crate::cli::statusline::data::{CountData, IcmData, ScopesData, StatusData, ThrottleData};
use crate::config::{Config, HashingMode, McpServer, Throttle};

/// The full `llmenv-status.json` document: the schema envelope (`$schema`,
/// `v`, `ts`) plus the stats themselves (flattened from [`StatusData`], so the
/// on-disk shape matches exactly what `StatusData::load` parses back).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusDataJson {
    #[serde(rename = "$schema")]
    schema: &'static str,
    v: u32,
    ts: String,
    #[serde(flatten)]
    data: StatusData,
}

/// Inputs for config-staleness comparison (Task 10b), already resolved by the
/// caller — see `collect_config_stale`'s doc comment for why this module
/// doesn't resolve them itself. `None` when the caller has no staleness
/// comparison available for this context (degrades `config_stale` to `None`,
/// not a false negative).
#[derive(Debug, Clone, Copy)]
pub struct ConfigStaleInputs<'a> {
    pub(crate) booted_hash: Option<&'a str>,
    pub(crate) current_hash: &'a str,
}

/// Gather every statusline stat. `cache_root` + `hashing` are needed for the
/// prunable-bytes dry-run scan; `throttle_configs` is the merged manifest
/// throttle list (top-level + bundle) for the fresh-resolve path — pass `&[]`
/// when unavailable (throttle then reads back only the last-stored state via
/// `crate::throttle::read_active_throttle`, skipping resolution against
/// current config). `bundle_mcp` is the merged manifest's bundle-contributed
/// MCP servers (`manifest.capabilities.mcp`) — MCPs declared under a bundle's
/// `mcp:` key resolve through a separate path (`resolve_bundle_mcps`) from
/// top-level `config.mcp`, and omitting it would silently undercount
/// `mcps.total` for any config using bundle-scoped MCP servers. Pass `&[]`
/// when unavailable. `config_stale` carries the already-resolved
/// booted/current manifest-hash pair for the config-staleness comparison
/// (`None` when the caller has no staleness comparison available for this
/// context — see `ConfigStaleInputs`'s doc comment for why this module
/// doesn't resolve the hashes itself).
#[must_use]
pub fn collect_status_data(
    config: &Config,
    active: &crate::scope::ActiveScopes,
    throttle_configs: &[Throttle],
    bundle_mcp: &[McpServer],
    config_stale: Option<ConfigStaleInputs<'_>>,
    cache_root: &Path,
    hashing: HashingMode,
) -> StatusDataJson {
    let (throttle, throttle_fault) = collect_throttle(throttle_configs, &active.tags);
    let (throttle_error, throttle_conflict) = match throttle_fault {
        None => (None, None),
        Some(ThrottleFault::Unreadable(e)) => (Some(e), None),
        Some(ThrottleFault::Conflict(e)) => (None, Some(e)),
    };
    StatusDataJson {
        schema: "llmenv-status-v1",
        v: 1,
        ts: current_timestamp(),
        data: StatusData {
            scopes: Some(ScopesData {
                tags: active.tags.iter().cloned().collect(),
            }),
            plugins: collect_plugins(config, &active.tags),
            mcps: collect_mcps(config, bundle_mcp, &active.tags),
            icm: collect_icm(),
            throttle,
            throttle_error,
            throttle_conflict,
            config_stale: config_stale
                .and_then(|inputs| collect_config_stale(inputs.booted_hash, inputs.current_hash)),
            cache: Some(collect_cache(cache_root, hashing)),
            session_log: collect_session_log(config),
            tasks: Some(collect_tasks()),
            unreadable: false,
        },
    }
}

/// This binary crate has no `chrono`/`time` dependency (`rg -n "^chrono|^time
/// " Cargo.toml` finds none). `ts` is informational only — a staleness
/// diagnostic never parsed by the renderer (see `StatusData`'s doc comment) —
/// so a bare Unix-epoch-seconds string is sufficient rather than pulling in a
/// date/time crate for one field.
fn current_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// Best-effort plugin count. Only top-level `config.plugin_collection` is
/// resolved here — bundle-contributed collections require the merged
/// manifest, which isn't available at this call site. Same simplification
/// `run_export` documents for its top-level-only memory check (#335).
///
/// Fail-fast resolver: a resolve error means at least one configured plugin
/// reference is broken. There's no per-plugin error accumulation today, so
/// this reports "1 error, 0 known-good" rather than fabricating a count.
fn collect_plugins(config: &Config, active_tags: &BTreeSet<String>) -> Option<CountData> {
    match crate::plugins::resolve::resolve_plugins(config, active_tags) {
        Ok(resolved) => Some(CountData {
            total: resolved.plugins.len() as u64,
            errors: 0,
        }),
        Err(e) => {
            // The status line shows only a count, so the cause goes to the log (#3109).
            tracing::warn!(
                "cannot resolve plugins for the status line: {e:#}. Fix: correct the plugin \
                 references in config.yaml."
            );
            Some(CountData {
                total: 0,
                errors: 1,
            })
        }
    }
}

/// Best-effort MCP server count. Sums both resolution paths: top-level
/// `config.mcp` (via `resolve_mcps`) and bundle-contributed `mcp:` blocks
/// (via `resolve_bundle_mcps`, on `bundle_mcp` — the merged manifest's
/// `capabilities.mcp`). Omitting the bundle path would silently undercount
/// (potentially to zero) for any config declaring MCP servers only through
/// bundles, a normal, well-supported configuration shape in this codebase.
fn collect_mcps(
    config: &Config,
    bundle_mcp: &[McpServer],
    active_tags: &BTreeSet<String>,
) -> Option<CountData> {
    let memory: &[_] = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let top_level =
        crate::mcp::resolve::resolve_mcps(&config.mcp, memory, &config.host, active_tags);
    let bundle = crate::mcp::resolve::resolve_bundle_mcps(bundle_mcp, active_tags);
    let codebase_memory_entries: &[_] = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    let codebase_memory = if codebase_memory_entries.is_empty() {
        Ok(Vec::new())
    } else {
        crate::mcp::resolve::codebase_memory_paths()
            .ok()
            .ok_or(())
            .and_then(|(project_root, state_dir)| {
                crate::mcp::resolve::resolve_codebase_memory_entries(
                    codebase_memory_entries,
                    active_tags,
                    &project_root,
                    &state_dir,
                )
                .map_err(|_| ())
            })
    };
    match (top_level, bundle, codebase_memory) {
        (Ok(top), Ok(bundle), Ok(codebase_memory)) => Some(CountData {
            total: (top.len() + bundle.len() + codebase_memory.len()) as u64,
            errors: 0,
        }),
        _ => Some(CountData {
            total: 0,
            errors: 1,
        }),
    }
}

/// Prefer resolving fresh against current config (reflects a same-render
/// change); fall back to the last-stored state if resolution finds nothing
/// (e.g. called from a context with no merged manifest handy).
fn collect_throttle(
    throttle_configs: &[Throttle],
    active_tags: &BTreeSet<String>,
) -> (Option<ThrottleData>, Option<ThrottleFault>) {
    collect_throttle_with_stored_fallback(
        throttle_configs,
        active_tags,
        crate::throttle::read_active_throttle,
    )
}

/// Internal helper taking an injectable "read the last-stored state" thunk so
/// tests can exercise the resolve/fallback branching without touching the
/// real state dir (mirrors the `_with_state_dir` DI pattern used in
/// `crate::throttle` and `crate::cli::statusline::icons`).
///
/// The second element is set when the throttle cannot be used. Neither case is "throttling off",
/// so the widget can show it: a config conflict, or a stored state that cannot be read.
fn collect_throttle_with_stored_fallback(
    throttle_configs: &[Throttle],
    active_tags: &BTreeSet<String>,
    read_stored: impl FnOnce() -> anyhow::Result<Option<Throttle>>,
) -> (Option<ThrottleData>, Option<ThrottleFault>) {
    let resolved = match crate::throttle::resolve_active_throttle(throttle_configs, active_tags) {
        Ok(resolved) => resolved,
        Err(e) => {
            tracing::error!("throttle resolution failed (non-fatal): {e:#}");
            return (None, Some(ThrottleFault::Conflict(format!("{e:#}"))));
        }
    };
    if let Some(t) = resolved {
        return (Some(throttle_data(t)), None);
    }
    match read_stored() {
        Ok(stored) => (stored.map(throttle_data), None),
        Err(e) => (None, Some(ThrottleFault::Unreadable(format!("{e:#}")))),
    }
}

/// Why a throttle cannot be shown. The widget labels the two cases differently: a config conflict
/// is not an unreadable state file.
#[derive(Debug, Clone, PartialEq)]
enum ThrottleFault {
    /// More than one throttle entry is active at once.
    Conflict(String),
    /// The stored throttle state exists but cannot be read.
    Unreadable(String),
}

fn throttle_data(t: Throttle) -> ThrottleData {
    ThrottleData {
        backend: t.backend,
        cooldown_secs: t.max_wait,
    }
}

/// Compare the manifest hash the agent booted with against the current one,
/// reusing `crate::cli::stale_status` (the same mechanism `llmenv check-stale`
/// uses) so there is exactly one staleness rule in the codebase. `booted` is
/// the `content_hash` from the booted folder's `.llmenv-manifest.json`
/// (`None` when there's no booted manifest to compare against — llmenv didn't
/// boot this agent, or the folder predates the manifest dotfile). `current`
/// is the freshly-computed manifest hash for what would be materialized now.
/// Pure function — no I/O; callers resolve both hashes themselves (see Task
/// 11/12 for how the materialize/export paths obtain them).
fn collect_config_stale(booted: Option<&str>, current: &str) -> Option<bool> {
    match crate::cli::stale_status(booted, current) {
        crate::cli::StaleStatus::Fresh => Some(false),
        crate::cli::StaleStatus::Stale { .. } => Some(true),
        crate::cli::StaleStatus::Unknown => None,
    }
}

/// Best-effort session-log line count, of the configured `session_log.file.path` or the default.
fn collect_session_log(config: &Config) -> Option<u64> {
    let configured = config.session_log_resolved();
    let path = match crate::session_log::resolve_file_path(configured.file_path()) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!(
                "session log stat collection failed, so the line count is empty: {e:#}. Fix: \
                 check session_log.file.path, and that the session log folder is readable."
            );
            return None;
        }
    };
    crate::session_log::count::count_records(&path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
