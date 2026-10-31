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
use cache::collect_cache;
use icm::collect_icm;
#[cfg(test)]
use icm::{extract_stat_line, parse_icm_stats};

use crate::cli::statusline::data::{
    CountData, IcmData, ScopesData, SessionProgress, StatusData, TasksData, ThrottleData,
};
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
        Err(_) => Some(CountData {
            total: 0,
            errors: 1,
        }),
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

/// Task-tracker progress, scoped to the sessions open for the current project (mandatory-sessions
/// design). `session` is `None` when zero sessions are open for this project (the widget renders
/// empty); `Some` carries the summed `(done, total)` across every session open for this project.
/// A state dir or cwd that cannot be resolved is the `unreadable` marker, not an empty section, so
/// the status line never reads as "no work" (#2687).
fn collect_tasks() -> TasksData {
    let unreadable = |what: &str, e: &dyn std::fmt::Display| {
        tracing::error!(
            "cannot read the tasks, {what} is unknown: {e}; the status line shows unreadable"
        );
        TasksData {
            session: None,
            current: None,
            unreadable: true,
        }
    };
    let state_dir = match crate::paths::state_dir() {
        Ok(dir) => dir,
        Err(e) => return unreadable("the state directory", &e),
    };
    match crate::task::project::current_tag() {
        Ok(project) => collect_tasks_from_state_dir(&state_dir, &project),
        Err(e) => unreadable("the project of the working directory", &e),
    }
}

/// Internal helper taking injectable state dir + project tag so tests can
/// exercise this without touching the real, ambient state dir or cwd.
fn collect_tasks_from_state_dir(state_dir: &Path, project: &str) -> TasksData {
    // The status line has no text channel for a warning, so the error goes to the log. The
    // widget shows the `unreadable` marker, so a read failure never looks like "no work".
    match try_tasks_data(state_dir, project) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the tasks; the status line shows unreadable");
            TasksData {
                session: None,
                current: None,
                unreadable: true,
            }
        }
    }
}

/// The task fields of the status line, read strictly: any unreadable session or task file is an
/// error.
fn try_tasks_data(state_dir: &Path, project: &str) -> anyhow::Result<TasksData> {
    let open_sessions = crate::task::session::open_sessions_strict(state_dir, project)?;
    let session = if open_sessions.is_empty() {
        None
    } else {
        let mut total_progress = (0, 0);
        for s in &open_sessions {
            let (d, t) = crate::task::session::try_session_progress(state_dir, &s.id)?;
            total_progress = (total_progress.0 + d, total_progress.1 + t);
        }
        Some(SessionProgress {
            done: total_progress.0,
            total: total_progress.1,
        })
    };
    let session_ids: Vec<String> = open_sessions.into_iter().map(|s| s.id).collect();
    let current = crate::task::current_wip_title(state_dir, &session_ids)?;
    Ok(TasksData {
        session,
        current,
        unreadable: false,
    })
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
mod tests {
    use super::*;
    use crate::config::{Features, Marketplace, Memory, PluginCollection};
    use crate::materialize::dir_size::dir_size;
    use proptest::prelude::*;

    fn tags(ts: &[&str]) -> BTreeSet<String> {
        ts.iter().map(|s| (*s).to_string()).collect()
    }

    fn active_scopes(ts: &[&str]) -> crate::scope::ActiveScopes {
        crate::scope::ActiveScopes {
            scopes: vec![],
            tags: tags(ts),
            ..Default::default()
        }
    }

    // --- collect_status_data: whole-pipeline smoke tests ---

    #[test]
    fn collect_status_data_populates_scopes_tags() {
        let config = Config::default();
        let active = active_scopes(&["dev", "rust"]);
        let dir = tempfile::tempdir().unwrap();
        let data = collect_status_data(
            &config,
            &active,
            &[],
            &[],
            None,
            dir.path(),
            HashingMode::default(),
        );
        let scopes = data.data.scopes.expect("scopes always populated");
        assert_eq!(scopes.tags, vec!["dev".to_string(), "rust".to_string()]);
    }

    #[test]
    fn collect_status_data_never_panics_on_empty_config() {
        let config = Config::default();
        let active = active_scopes(&[]);
        let dir = tempfile::tempdir().unwrap();
        // Must not panic even though: no plugins configured, no cache dir
        // exists yet. `icm` isn't asserted here — `collect_icm` resolves the
        // *host's* real config/backend (no DI seam), so its value depends on
        // whatever ICM setup happens to be active on the machine running the
        // test; `parse_icm_stats`'s own tests below cover its logic
        // hermetically.
        let data = collect_status_data(
            &config,
            &active,
            &[],
            &[],
            None,
            dir.path(),
            HashingMode::default(),
        );
        // Empty config resolves cleanly (no error), not "resolve failed".
        assert_eq!(
            data.data.plugins,
            Some(CountData {
                total: 0,
                errors: 0
            })
        );
        assert_eq!(
            data.data.mcps,
            Some(CountData {
                total: 0,
                errors: 0
            })
        );
    }

    #[test]
    fn serializes_to_expected_json_shape() {
        let config = Config::default();
        let active = active_scopes(&[]);
        let dir = tempfile::tempdir().unwrap();
        let data = collect_status_data(
            &config,
            &active,
            &[],
            &[],
            None,
            dir.path(),
            HashingMode::default(),
        );
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["$schema"], "llmenv-status-v1");
        assert_eq!(json["v"], 1);
        assert!(json.get("ts").is_some());
        // Flattened, not nested under a "data" key.
        assert!(json.get("data").is_none());
        assert!(json.get("scopes").is_some());
    }

    // --- collect_plugins ---

    #[test]
    fn collect_plugins_no_plugins_configured_is_zero_not_error() {
        let config = Config::default();
        let result = collect_plugins(&config, &tags(&[]));
        assert_eq!(
            result,
            Some(CountData {
                total: 0,
                errors: 0
            })
        );
    }

    #[test]
    fn collect_plugins_degrades_to_error_count_on_resolve_failure() {
        // References an undeclared marketplace — resolve_plugins errors.
        let config = Config {
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec!["t".into()],
                plugins: vec!["ghost:plugin".into()],
            }],
            ..Config::default()
        };
        let result = collect_plugins(&config, &tags(&["t"]));
        assert_eq!(
            result,
            Some(CountData {
                total: 0,
                errors: 1
            }),
            "broken plugin ref must degrade to an error count, not panic or silently show 0 errors"
        );
    }

    #[test]
    fn collect_plugins_counts_resolved_plugins() {
        let config = Config {
            marketplace: vec![Marketplace {
                name: "mk".into(),
                source: "https://example.com/mk".into(),
            }],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec!["t".into()],
                plugins: vec!["mk:one".into(), "mk:two".into()],
            }],
            ..Config::default()
        };
        let result = collect_plugins(&config, &tags(&["t"]));
        assert_eq!(
            result,
            Some(CountData {
                total: 2,
                errors: 0
            })
        );
    }

    // --- collect_mcps ---

    #[test]
    fn collect_mcps_no_servers_configured_is_zero_not_error() {
        let config = Config::default();
        let result = collect_mcps(&config, &[], &tags(&[]));
        assert_eq!(
            result,
            Some(CountData {
                total: 0,
                errors: 0
            })
        );
    }

    #[test]
    fn collect_mcps_counts_active_codebase_memory_entries() {
        let config = Config {
            features: Some(crate::config::Features {
                codebase_memory: vec![crate::config::CodebaseMemory {
                    allowed_roots: vec![],
                    mem_budget_mb: None,
                    when: vec!["proj".to_string()],
                    index_path: None,
                    mcp_permissions: None,
                }],
                ..Default::default()
            }),
            ..Config::default()
        };
        let result = collect_mcps(&config, &[], &tags(&["proj"]));
        assert_eq!(
            result,
            Some(CountData {
                total: 1,
                errors: 0
            })
        );
    }

    #[test]
    fn collect_mcps_counts_bundle_contributed_servers_with_no_top_level_config() {
        // A config with zero top-level `mcp:` entries but a bundle-contributed
        // server must NOT report zero — that's exactly the undercount this
        // task's review caught (bundle-only MCP configs are a normal shape).
        let config = Config::default();
        let bundle_mcp = vec![McpServer {
            name: "playwright".to_string(),
            when: vec!["dev".to_string()],
            command: Some("npx".to_string()),
            ..Default::default()
        }];
        let result = collect_mcps(&config, &bundle_mcp, &tags(&["dev"]));
        assert_eq!(
            result,
            Some(CountData {
                total: 1,
                errors: 0
            })
        );
    }

    #[test]
    fn collect_mcps_degrades_to_error_count_on_ambiguous_memory() {
        let mem = |host: &str| Memory {
            server_host: host.into(),
            port: 9092,
            listen_host: "127.0.0.1".into(),
            when: vec!["t".into()],
            global_topics: vec![],
            ..Memory::default()
        };
        let config = Config {
            features: Some(Features {
                memory: vec![mem("a"), mem("b")],
                ..Features::default()
            }),
            ..Config::default()
        };
        let result = collect_mcps(&config, &[], &tags(&["t"]));
        assert_eq!(
            result,
            Some(CountData {
                total: 0,
                errors: 1
            }),
            "ambiguous memory config must degrade to an error count, not panic"
        );
    }

    // --- collect_icm / parse_icm_stats ---

    #[test]
    fn collect_icm_never_panics() {
        // `collect_icm` resolves the *host's* real config/backend (no DI
        // seam) — its return value depends on whatever ICM setup is active
        // on the machine running the test, so this only asserts the
        // documented degrade-cleanly contract, not a specific outcome.
        // `parse_icm_stats`'s tests below cover the parsing logic hermetically.
        let _ = collect_icm();
    }

    #[test]
    fn parse_icm_stats_extracts_memories_and_concepts() {
        let result =
            parse_icm_stats("Memories: 142\nTopics: 47\nAvg weight: 0.3\nOldest: x\nNewest: y\n");
        assert_eq!(
            result,
            Some(IcmData {
                memories: 142,
                concepts: Some(47)
            })
        );
    }

    #[test]
    fn parse_icm_stats_leaves_concepts_unknown_when_absent() {
        let result = parse_icm_stats("Memories: 10\n");
        assert_eq!(
            result,
            Some(IcmData {
                memories: 10,
                concepts: None
            })
        );
    }

    #[test]
    fn parse_icm_stats_returns_none_on_unrecognized_text() {
        assert!(parse_icm_stats("not a stats response at all").is_none());
    }

    #[test]
    fn parse_icm_stats_returns_none_when_memories_missing() {
        assert!(parse_icm_stats("Topics: 5\n").is_none());
    }

    proptest! {
        /// Roundtrip: any `memories`/`concepts` pair formatted as the real
        /// `icm_memory_stats` text response must parse back to the same
        /// `IcmData`, with `concepts` unknown (`None`) when its "Topics" line
        /// is omitted.
        #[test]
        fn parse_icm_stats_roundtrips_arbitrary_values(
            memories in any::<u64>(),
            concepts in proptest::option::of(any::<u64>()),
        ) {
            let raw = match concepts {
                Some(c) => format!("Memories: {memories}\nTopics: {c}\n"),
                None => format!("Memories: {memories}\n"),
            };
            let result = parse_icm_stats(&raw);
            prop_assert_eq!(
                result,
                Some(IcmData {
                    memories,
                    concepts,
                })
            );
        }

        /// Arbitrary, likely-malformed input must never panic — only ever
        /// `None` or a valid `IcmData`.
        #[test]
        fn parse_icm_stats_never_panics_on_arbitrary_input(raw in ".{0,200}") {
            let _ = parse_icm_stats(&raw);
        }

        /// Roundtrip: a well-formed "<label>: <n>" line, anywhere among
        /// arbitrary surrounding lines, must parse back to `n` — whitespace
        /// around the colon and extra lines before/after don't matter.
        #[test]
        fn extract_stat_line_roundtrips_well_formed_line(
            label in "[A-Za-z]{1,20}",
            n in any::<u64>(),
            colon_spaces in " {0,3}",
            before in proptest::collection::vec("[A-Za-z0-9]{0,20}", 0..3),
            after in proptest::collection::vec("[A-Za-z0-9]{0,20}", 0..3),
        ) {
            let mut lines = before;
            lines.push(format!("{label}{colon_spaces}: {n}"));
            lines.extend(after);
            let raw = lines.join("\n");
            prop_assert_eq!(extract_stat_line(&raw, &label), Some(n));
        }

        /// Arbitrary, likely-malformed input must never panic — only ever
        /// `None` or a correctly-parsed `u64`.
        #[test]
        fn extract_stat_line_never_panics_on_arbitrary_input(
            raw in ".{0,200}",
            label in "[A-Za-z]{0,20}",
        ) {
            let _ = extract_stat_line(&raw, &label);
        }

        /// A label that's a substring of another line's label (e.g. "Mem"
        /// vs "Memories") must not false-match — `strip_prefix` only
        /// accepts an exact-label-then-colon boundary.
        #[test]
        fn extract_stat_line_does_not_match_label_substring_collisions(
            n in any::<u64>(),
        ) {
            let raw = format!("Memories: {n}\n");
            prop_assert_eq!(extract_stat_line(&raw, "Mem"), None);
        }
    }

    // --- collect_throttle ---

    fn throttle_cfg(backend: &str, tag: &str, max_wait: u64) -> Throttle {
        Throttle {
            backend: backend.into(),
            when: vec![tag.into()],
            cache_ttl: 30,
            max_wait,
            soft_threshold: 20,
        }
    }

    #[test]
    fn collect_throttle_prefers_fresh_resolve_over_stored() {
        let configs = vec![throttle_cfg("fresh", "t", 60)];
        let result = collect_throttle_with_stored_fallback(&configs, &tags(&["t"]), || {
            panic!("stored fallback must not be consulted when resolution succeeds")
        });
        assert_eq!(
            result,
            (
                Some(ThrottleData {
                    backend: "fresh".into(),
                    cooldown_secs: 60
                }),
                None
            )
        );
    }

    #[test]
    fn collect_throttle_falls_back_to_stored_when_nothing_resolves() {
        let stored = throttle_cfg("stored", "unused-tag", 45);
        let result = collect_throttle_with_stored_fallback(&[], &tags(&["t"]), || Ok(Some(stored)));
        assert_eq!(
            result,
            (
                Some(ThrottleData {
                    backend: "stored".into(),
                    cooldown_secs: 45
                }),
                None
            )
        );
    }

    #[test]
    fn collect_throttle_reports_an_unresolvable_throttle_instead_of_off() {
        // Two entries active at once make resolution fail. That must read as an error, not
        // as "off", and the stored state must not hide it.
        let configs = vec![throttle_cfg("a", "t", 60), throttle_cfg("b", "t", 30)];
        let (throttle, error) =
            collect_throttle_with_stored_fallback(&configs, &tags(&["t"]), || {
                panic!("stored fallback must not mask a failed resolution")
            });
        assert_eq!(throttle, None);
        let fault = error.expect("a failed resolution must set a throttle fault");
        assert!(
            matches!(&fault, ThrottleFault::Conflict(text) if text.contains("conflicting backends")),
            "a config conflict is not an unreadable state: {fault:?}"
        );
    }

    #[test]
    fn collect_throttle_none_when_neither_resolves_nor_stored() {
        let result = collect_throttle_with_stored_fallback(&[], &tags(&["t"]), || Ok(None));
        assert_eq!(result, (None, None));
    }

    #[test]
    fn collect_throttle_reports_unreadable_stored_state_as_error() {
        let result = collect_throttle_with_stored_fallback(&[], &tags(&["t"]), || {
            Err(anyhow::anyhow!(
                "parsing throttle state: /state/throttle.json"
            ))
        });
        assert_eq!(result.0, None);
        let fault = result.1.expect("unreadable state must surface a fault");
        assert!(
            matches!(&fault, ThrottleFault::Unreadable(text) if text.contains("/state/throttle.json")),
            "unreadable state names the file: {fault:?}"
        );
    }

    // --- collect_config_stale ---

    #[test]
    fn collect_config_stale_none_when_no_booted_hash() {
        // No booted manifest to compare against (llmenv didn't boot this agent,
        // or the booted folder predates the manifest dotfile) — must be None,
        // not a false "fresh" or "stale".
        assert_eq!(collect_config_stale(None, "current-hash"), None);
    }

    #[test]
    fn collect_config_stale_false_when_hashes_match() {
        assert_eq!(
            collect_config_stale(Some("same-hash"), "same-hash"),
            Some(false)
        );
    }

    #[test]
    fn collect_config_stale_true_when_hashes_differ() {
        assert_eq!(
            collect_config_stale(Some("old-hash"), "new-hash"),
            Some(true)
        );
    }

    // --- collect_cache / dir_size ---

    #[test]
    fn dir_size_fails_on_a_stat_error_instead_of_reporting_zero() {
        // A child of a regular file fails with ENOTDIR, which is not a missing path (#3038).
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").expect("write fixture file");
        assert!(dir_size(&file.join("child")).is_err());
    }

    #[test]
    fn dir_size_sums_nested_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"12345").unwrap(); // 5 bytes
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("b.txt"), b"1234567890").unwrap(); // 10 bytes
        assert_eq!(dir_size(dir.path()).expect("dir size").bytes, 15);
    }

    /// Writes one file per size in `sizes`, named `<prefix><index>.bin`, each
    /// filled with that many zero bytes.
    fn write_sized_files(dir: &Path, prefix: &str, sizes: &[usize]) {
        for (i, size) in sizes.iter().enumerate() {
            std::fs::write(dir.join(format!("{prefix}{i}.bin")), vec![0u8; *size]).unwrap();
        }
    }

    proptest! {
        /// Accumulation correctness across generated directory trees (flat +
        /// one nested level) — must equal the sum of every file's byte count.
        #[test]
        fn dir_size_sums_arbitrary_generated_directory_trees(
            flat_sizes in prop::collection::vec(0usize..2000, 0..8),
            nested_sizes in prop::collection::vec(0usize..2000, 0..8),
        ) {
            let dir = tempfile::tempdir().unwrap();
            write_sized_files(dir.path(), "f", &flat_sizes);
            let nested = dir.path().join("nested");
            std::fs::create_dir(&nested).unwrap();
            write_sized_files(&nested, "n", &nested_sizes);
            let expected: u64 = flat_sizes
                .iter()
                .chain(nested_sizes.iter())
                .map(|s| *s as u64)
                .sum();
            prop_assert_eq!(dir_size(dir.path()).expect("dir size").bytes, expected);
        }

        /// Adding a file must monotonically increase the reported size by
        /// exactly the new file's byte count.
        #[test]
        fn dir_size_is_monotonic_when_adding_a_file(
            initial_sizes in prop::collection::vec(0usize..2000, 0..8),
            extra_size in 0usize..2000,
        ) {
            let dir = tempfile::tempdir().unwrap();
            write_sized_files(dir.path(), "f", &initial_sizes);
            let before = dir_size(dir.path()).expect("dir size").bytes;
            std::fs::write(dir.path().join("extra.bin"), vec![0u8; extra_size]).unwrap();
            let after = dir_size(dir.path()).expect("dir size").bytes;
            prop_assert!(after >= before);
            prop_assert_eq!(after - before, extra_size as u64);
        }
    }

    // --- collect_session_log ---

    #[test]
    fn collect_session_log_from_path_counts_nonempty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "line one\nline two\n\nline three\n").unwrap();
        assert_eq!(crate::session_log::count::count_records(&path), Some(3));
    }

    #[test]
    fn collect_session_log_from_path_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.jsonl");
        assert_eq!(crate::session_log::count::count_records(&missing), None);
    }

    // --- collect_tasks (mandatory-sessions rework) ---

    use crate::task::session::{StartDecision, StartOutcome, start_session};

    const PROJECT: &str = "proj-0000000000";
    const OTHER: &str = "other-1111111111";

    fn created(outcome: StartOutcome) -> crate::task::session::Session {
        match outcome {
            StartOutcome::Created(s) => s,
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[test]
    fn collect_tasks_an_unreadable_session_store_shows_the_marker_not_no_work() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        created(
            crate::task::session::start_session(
                dir.path(),
                None,
                None,
                PROJECT,
                crate::task::session::StartDecision::Auto,
            )
            .unwrap(),
        );
        let sessions = crate::task::tasks_dir(dir.path()).join("sessions");
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = std::fs::read_dir(&sessions).is_ok();
        let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_anyway {
            return; // running as root: the permission cannot be enforced
        }
        assert!(
            data.unreadable,
            "an unreadable store must not look like no work"
        );
        assert!(data.session.is_none());
    }

    #[test]
    fn collect_tasks_zero_open_sessions_for_project_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
        assert!(data.session.is_none());
        assert!(data.current.is_none());
    }

    #[test]
    fn collect_tasks_one_open_session_reports_done_over_total() {
        let dir = tempfile::tempdir().unwrap();
        let session = created(
            start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).unwrap(),
        );
        let t1 = crate::task::add_task_for_session(
            dir.path(),
            "one",
            crate::task::ParentSpec::Detached,
            &session.id,
        )
        .unwrap();
        crate::task::add_task_for_session(
            dir.path(),
            "two",
            crate::task::ParentSpec::Detached,
            &session.id,
        )
        .unwrap();
        crate::task::done_task(dir.path(), &t1.slug).unwrap();

        let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
        assert_eq!(data.session, Some(SessionProgress { done: 1, total: 2 }));
    }

    #[test]
    fn collect_tasks_two_open_sessions_sums_across_project_only() {
        let dir = tempfile::tempdir().unwrap();
        let s1 = created(
            start_session(
                dir.path(),
                Some("first"),
                None,
                PROJECT,
                StartDecision::Auto,
            )
            .unwrap(),
        );
        let s2 = created(
            start_session(
                dir.path(),
                Some("second"),
                None,
                PROJECT,
                StartDecision::New,
            )
            .unwrap(),
        );
        start_session(
            dir.path(),
            Some("unrelated"),
            None,
            OTHER,
            StartDecision::Auto,
        )
        .unwrap();

        let t1 = crate::task::add_task_for_session(
            dir.path(),
            "a",
            crate::task::ParentSpec::Detached,
            &s1.id,
        )
        .unwrap();
        crate::task::done_task(dir.path(), &t1.slug).unwrap();
        crate::task::add_task_for_session(
            dir.path(),
            "b",
            crate::task::ParentSpec::Detached,
            &s2.id,
        )
        .unwrap();

        let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
        assert_eq!(data.session, Some(SessionProgress { done: 1, total: 2 }));
    }

    #[test]
    fn collect_tasks_current_is_scoped_to_the_projects_open_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let session = created(
            start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).unwrap(),
        );
        let task = crate::task::add_task_for_session(
            dir.path(),
            "In progress",
            crate::task::ParentSpec::Detached,
            &session.id,
        )
        .unwrap();
        crate::task::start_task(dir.path(), &task.slug, false).unwrap();

        let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
        assert_eq!(data.current.as_deref(), Some("In progress"));
    }

    // A configured session_log.file.path is the file the widget counts, not the default one.
    #[test]
    fn the_session_log_count_reads_the_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        let yaml = format!(
            "session_log:\n  file:\n    enabled: true\n    path: {}\n",
            path.display()
        );
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(collect_session_log(&config), Some(3));
    }
}
