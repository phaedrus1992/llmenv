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

use crate::cli::statusline::data::{
    CacheData, CountData, IcmData, ScopesData, StatusData, ThrottleData,
};
use crate::config::{Config, HashingMode, McpServer, Throttle};

/// The full `llmenv-status.json` document: the schema envelope (`$schema`,
/// `v`, `ts`) plus the stats themselves (flattened from [`StatusData`], so the
/// on-disk shape matches exactly what `StatusData::load` parses back).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusDataJson {
    #[serde(rename = "$schema")]
    pub schema: &'static str,
    pub v: u32,
    pub ts: String,
    #[serde(flatten)]
    pub data: StatusData,
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
/// when unavailable.
#[must_use]
pub fn collect_status_data(
    config: &Config,
    active: &crate::scope::ActiveScopes,
    throttle_configs: &[Throttle],
    bundle_mcp: &[McpServer],
    cache_root: &Path,
    hashing: HashingMode,
) -> StatusDataJson {
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
            throttle: collect_throttle(throttle_configs, &active.tags),
            config_stale: collect_config_stale(cache_root),
            cache: collect_cache(cache_root, hashing),
            session_log: collect_session_log(),
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
    match (top_level, bundle) {
        (Ok(top), Ok(bundle)) => Some(CountData {
            total: (top.len() + bundle.len()) as u64,
            errors: 0,
        }),
        _ => Some(CountData {
            total: 0,
            errors: 1,
        }),
    }
}

/// This collector runs on hot paths (materialize, `llmenv export`, session
/// start) — not an interactive CLI invocation where a user is watching and
/// waiting is fine. A slow/unreachable ICM backend must not stall one of
/// those paths, so this uses the same short timeout the hook-run MCP calls
/// use rather than the 10s interactive-CLI timeout `memory::stats_json` uses.
const ICM_STATS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Best-effort live ICM query. Returns `None` when no memory backend is
/// active for the current scope, the MCP call fails or times out, or the
/// response can't be parsed — every one of those is an expected, non-error
/// condition (most sessions have no ICM backend configured at all).
fn collect_icm() -> Option<IcmData> {
    let raw = crate::memory::stats_json_with_timeout(ICM_STATS_TIMEOUT).ok()?;
    parse_icm_stats(&raw)
}

/// Pure JSON-extraction half of `collect_icm`, split out so it's testable
/// without a live ICM MCP backend.
fn parse_icm_stats(raw: &str) -> Option<IcmData> {
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some(IcmData {
        memories: parsed.get("memories")?.as_u64()?,
        concepts: parsed
            .get("concepts")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    })
}

/// Prefer resolving fresh against current config (reflects a same-render
/// change); fall back to the last-stored state if resolution finds nothing
/// (e.g. called from a context with no merged manifest handy).
fn collect_throttle(
    throttle_configs: &[Throttle],
    active_tags: &BTreeSet<String>,
) -> Option<ThrottleData> {
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
fn collect_throttle_with_stored_fallback(
    throttle_configs: &[Throttle],
    active_tags: &BTreeSet<String>,
    read_stored: impl FnOnce() -> anyhow::Result<Option<Throttle>>,
) -> Option<ThrottleData> {
    let resolved = crate::throttle::resolve_active_throttle(throttle_configs, active_tags)
        .ok()
        .flatten()
        .or_else(|| read_stored().ok().flatten());
    resolved.map(|t| ThrottleData {
        backend: t.backend,
        cooldown_secs: t.max_wait,
    })
}

/// Stub — Task 10b (a follow-up task in this same plan) wires this up to the
/// real `stale_status`/`StaleStatus` mechanism in `src/cli/mod.rs`. Always
/// `None` until then; this is deliberate scoping, not an oversight.
fn collect_config_stale(cache_root: &Path) -> Option<bool> {
    let _ = cache_root;
    None
}

/// Best-effort prunable-bytes estimate: a dry-run `StaleOnly` prune scan, so
/// no filesystem mutation ever happens just to report a stat.
fn collect_cache(cache_root: &Path, hashing: HashingMode) -> Option<CacheData> {
    let current_version =
        matches!(hashing, HashingMode::Normal).then(crate::materialize::cache::version_mm);
    let report = crate::materialize::cache::prune(
        cache_root,
        crate::materialize::cache::PruneMode::StaleOnly,
        hashing,
        current_version.as_deref(),
        true, // dry_run: must not delete anything just to report a stat
    )
    .ok()?;
    let prunable_bytes: u64 = report.removed.iter().map(|p| dir_size(p)).sum();
    Some(CacheData { prunable_bytes })
}

/// Recursively sum the on-disk size of every entry under `path`.
///
/// `prune`'s `removed` list holds directory (or symlink) paths, not files —
/// stat'ing the directory entry itself would report a few KiB regardless of
/// the tree's actual contents, so this walks in and sums real file sizes.
/// Symlinks are sized by their own on-disk length rather than followed,
/// matching `prune`'s own care not to traverse through them. Unreadable
/// entries are skipped (best-effort estimate, not exact accounting).
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return meta.len();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| dir_size(&entry.path()))
        .sum()
}

/// Best-effort session-log line count.
fn collect_session_log() -> Option<u64> {
    let path = crate::session_log::file_sink::default_file_path().ok()?;
    collect_session_log_from_path(&path)
}

/// Internal helper taking an injectable path so tests can point at a temp
/// file instead of the real, ambient state dir.
fn collect_session_log_from_path(path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().filter(|l| !l.trim().is_empty()).count() as u64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::{Features, Marketplace, Memory, PluginCollection};
    use std::collections::BTreeMap;

    fn tags(ts: &[&str]) -> BTreeSet<String> {
        ts.iter().map(|s| (*s).to_string()).collect()
    }

    fn active_scopes(ts: &[&str]) -> crate::scope::ActiveScopes {
        crate::scope::ActiveScopes {
            scopes: vec![],
            tags: tags(ts),
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
        // Must not panic even though: no plugins configured, no memory
        // backend active (icm stays None), no cache dir exists yet.
        let data = collect_status_data(
            &config,
            &active,
            &[],
            &[],
            dir.path(),
            HashingMode::default(),
        );
        assert!(
            data.data.icm.is_none(),
            "no ICM backend active — must degrade to None, not error"
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
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: BTreeMap::new(),
            retention: None,
            auto_prune: false,
            consolidation: None,
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
    fn collect_icm_returns_none_when_no_backend_active() {
        // No memory backend configured for this scope anywhere in the test
        // process — connect() fails, so this must degrade cleanly.
        assert!(collect_icm().is_none());
    }

    #[test]
    fn parse_icm_stats_extracts_memories_and_concepts() {
        let result = parse_icm_stats(r#"{"memories": 142, "concepts": 47}"#);
        assert_eq!(
            result,
            Some(IcmData {
                memories: 142,
                concepts: 47
            })
        );
    }

    #[test]
    fn parse_icm_stats_defaults_concepts_to_zero_when_absent() {
        let result = parse_icm_stats(r#"{"memories": 10}"#);
        assert_eq!(
            result,
            Some(IcmData {
                memories: 10,
                concepts: 0
            })
        );
    }

    #[test]
    fn parse_icm_stats_returns_none_on_malformed_json() {
        assert!(parse_icm_stats("{ not valid json").is_none());
    }

    #[test]
    fn parse_icm_stats_returns_none_when_memories_missing() {
        assert!(parse_icm_stats(r#"{"concepts": 5}"#).is_none());
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
            Some(ThrottleData {
                backend: "fresh".into(),
                cooldown_secs: 60
            })
        );
    }

    #[test]
    fn collect_throttle_falls_back_to_stored_when_nothing_resolves() {
        let stored = throttle_cfg("stored", "unused-tag", 45);
        let result = collect_throttle_with_stored_fallback(&[], &tags(&["t"]), || Ok(Some(stored)));
        assert_eq!(
            result,
            Some(ThrottleData {
                backend: "stored".into(),
                cooldown_secs: 45
            })
        );
    }

    #[test]
    fn collect_throttle_none_when_neither_resolves_nor_stored() {
        let result = collect_throttle_with_stored_fallback(&[], &tags(&["t"]), || Ok(None));
        assert!(result.is_none());
    }

    // --- collect_config_stale (stub for this task; Task 10b implements it) ---

    #[test]
    fn collect_config_stale_always_none_in_this_task() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(collect_config_stale(dir.path()), None);
    }

    // --- collect_cache / dir_size ---

    #[test]
    fn dir_size_sums_nested_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"12345").unwrap(); // 5 bytes
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("b.txt"), b"1234567890").unwrap(); // 10 bytes
        assert_eq!(dir_size(dir.path()), 15);
    }

    #[test]
    fn collect_cache_missing_cache_root_yields_zero_prunable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let result = collect_cache(&missing, HashingMode::Strict);
        assert_eq!(result, Some(CacheData { prunable_bytes: 0 }));
    }

    #[test]
    fn collect_cache_reports_prunable_bytes_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        // A stale folder (doesn't match the current VERSION_TAG prefix).
        let stale = dir.path().join("0.0.1-old-deadbeef");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("file.txt"), b"0123456789").unwrap(); // 10 bytes

        let result = collect_cache(dir.path(), HashingMode::Strict);
        assert_eq!(result, Some(CacheData { prunable_bytes: 10 }));
        // dry_run: the stale folder must still be on disk afterward.
        assert!(
            stale.exists(),
            "collect_cache must never delete anything just to report a stat"
        );
    }

    // --- collect_session_log ---

    #[test]
    fn collect_session_log_from_path_counts_nonempty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "line one\nline two\n\nline three\n").unwrap();
        assert_eq!(collect_session_log_from_path(&path), Some(3));
    }

    #[test]
    fn collect_session_log_from_path_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.jsonl");
        assert_eq!(collect_session_log_from_path(&missing), None);
    }
}
