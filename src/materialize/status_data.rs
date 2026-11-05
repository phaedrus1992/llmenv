//! Collects the fields written to `llmenv-status.json` (the statusline data
//! file). All I/O here is best-effort: a sub-collector that can't get its
//! data (no ICM backend active, cache dir not yet materialized, etc.)
//! degrades to `None` (or a well-defined zero value) rather than failing the
//! whole collection — materialize/export must never abort because a stat is
//! unavailable.
//!
//! The JSON shape (`StatusData` and its fields, below) is defined in this
//! module — the writer side — since `cli::statusline` (the reader) only
//! parses it back; keeping one definition avoids a second, parallel schema
//! that could drift.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

mod cache;
mod icm;
pub mod session_tasks;
mod tasks;
use crate::config::{Config, HashingMode, McpServer, Throttle};
use cache::collect_cache;
use icm::collect_icm;
#[cfg(test)]
use icm::{extract_stat_line, parse_icm_stats};
pub use session_tasks::SessionTasks;
use tasks::collect_tasks;
#[cfg(test)]
use tasks::collect_tasks_from_state_dir;

// `Serialize` is derived alongside `Deserialize` on every type in this module
// (not just `StatusData`) so this module — the writer side — can construct
// and serialize these exact types, and `cli::statusline::data` — the reader
// side — can parse them back without a second, parallel set of structs that
// could drift out of sync.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct StatusData {
    pub scopes: Option<ScopesData>,
    pub plugins: Option<CountData>,
    pub mcps: Option<CountData>,
    pub icm: Option<IcmData>,
    pub throttle: Option<ThrottleData>,
    /// Set when the stored throttle state exists but cannot be read. The
    /// widget shows this instead of reporting throttling as off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_error: Option<String>,
    /// Set when more than one throttle entry is active at once. This is a config conflict, not an
    /// unreadable state file, so the widget labels it apart from `throttle_error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_conflict: Option<String>,
    pub config_stale: Option<bool>,
    pub cache: Option<CacheData>,
    pub session_log: Option<u64>,
    pub tasks: Option<TasksData>,
    /// Set by [`StatusData::load`] when the file exists but cannot be read or parsed. It is
    /// never written to the file, so a file cannot set it.
    #[serde(skip)]
    pub unreadable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ScopesData {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CountData {
    pub total: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct IcmData {
    pub memories: u64,
    /// `None` when the stats text has no Topics line. That count is unknown, not zero.
    pub concepts: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ThrottleData {
    pub backend: String,
    pub cooldown_secs: u64,
}

/// Prunable cache size. `prunable_bytes` is `None` when the size could not be
/// read, so the status line shows an unknown marker rather than a zero.
/// `prunable_partial` is true when some entry was left out, so the total is a
/// floor and the status line shows it as "at least" (#3069). Older status data
/// has no such field, and it reads as an exact figure.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CacheData {
    pub prunable_bytes: Option<u64>,
    #[serde(default)]
    pub prunable_partial: bool,
}

/// Task-tracker progress, scoped to sessions open for the current project
/// (mandatory-sessions design). `session` is `None` when zero sessions are
/// open for this project (render empty — no active work tracked here);
/// `Some` gives the summed `(done, total)` across every session open for
/// this project (a single open session's own totals when there's just one).
/// `current` is the title of the most recently updated `wip`/`waiting` task
/// among those sessions — `None` when nothing is currently in progress.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TasksData {
    pub session: Option<SessionProgress>,
    pub current: Option<String>,
    /// Set when the task or session files cannot be read. The widget shows this instead of
    /// an empty or lower count, which would look like no work (#2687).
    #[serde(default)]
    pub unreadable: bool,
    /// Each open session of the project. Empty in a file from an older llmenv (#3196).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionTasks>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct SessionProgress {
    pub done: u64,
    pub total: u64,
}

impl StatusData {
    /// Load and parse `llmenv-status.json` at `path`. Never fails: every problem yields
    /// `StatusData::default()` (every field `None`), so the renderer's llmenv-sourced widgets
    /// render empty rather than aborting the whole statusline. A missing file is the normal
    /// empty case. Any other failure is also logged at error level, which the default log
    /// filter shows, so an unreadable status file does not pass as no data (#2712).
    #[must_use]
    pub fn load(path: &std::path::Path) -> Self {
        match crate::paths::bounded::state_bytes_io(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::error!("status file {} is not valid JSON: {e}", path.display());
                Self::unreadable()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::error!("cannot read status file {}: {e}", path.display());
                Self::unreadable()
            }
        }
    }

    /// The default data with [`StatusData::unreadable`] set, for a file that exists but fails.
    fn unreadable() -> Self {
        Self {
            unreadable: true,
            ..Self::default()
        }
    }
}

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
///
/// The statusline calls this on each render, so a broken plugin reference logs
/// a warning on each render (#3109). The status line has no text channel for
/// the cause, and the log is the only place it can be seen.
pub(crate) fn collect_plugins(
    config: &Config,
    active_tags: &BTreeSet<String>,
) -> Option<CountData> {
    match crate::plugins::resolve::resolve_plugins(config, active_tags, false) {
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
/// reusing `stale_status` (the same mechanism `llmenv check-stale` uses) so
/// there is exactly one staleness rule in the codebase. `booted` is the
/// `content_hash` from the booted folder's `.llmenv-manifest.json` (`None`
/// when there's no booted manifest to compare against — llmenv didn't boot
/// this agent, or the folder predates the manifest dotfile). `current` is
/// the freshly-computed manifest hash for what would be materialized now.
/// Pure function — no I/O; callers resolve both hashes themselves (see Task
/// 11/12 for how the materialize/export paths obtain them).
fn collect_config_stale(booted: Option<&str>, current: &str) -> Option<bool> {
    match crate::materialize::stale::stale_status(booted, current) {
        crate::materialize::stale::StaleStatus::Fresh => Some(false),
        crate::materialize::stale::StaleStatus::Stale { .. } => Some(true),
        crate::materialize::stale::StaleStatus::Unknown => None,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod load_tests;
