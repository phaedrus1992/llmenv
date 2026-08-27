//! Resolve the merged manifest for a set of firing bundles — MCP servers,
//! plugins, memory, host, and throttle — without writing anything. Shared by
//! `cli`'s `regenerate`/`export`/`doctor` commands and by
//! `materialize::report_if_stale`'s drift check, so hook_run's drift check
//! doesn't need to depend on `cli` for it (that dependency is exactly what
//! the crate-coupling cycle work broke).

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::bundle_select::build_bundle_refs;
use crate::config::{Bundle, Config};
use crate::merge::MergedManifest;
use crate::scope::ActiveScopes;

/// Build the merged manifest for the firing bundles, resolving MCP servers and
/// plugins exactly as `cli::build_and_materialize` does — but without writing
/// anything. Returns `Ok(None)` when no firing bundle has a content directory.
/// The returned `cache_root` is the expanded cache dir (shared across adapters).
pub(crate) fn build_manifest(
    config: &Config,
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
    refresh_marketplaces: bool,
) -> anyhow::Result<Option<(MergedManifest, PathBuf)>> {
    let refs = build_bundle_refs(config_dir, active, firing);
    if refs.is_empty() {
        return Ok(None);
    }

    let mut manifest: MergedManifest =
        crate::merge::merge(&config.capabilities, &config.native, &refs)?;

    // Root-level lsp/skills: chain into manifest.capabilities (#661), mirroring memory/throttle.
    manifest.capabilities.lsp.extend(config.lsp.iter().cloned());
    manifest
        .capabilities
        .skills
        .extend(config.skills.iter().cloned());
    manifest
        .capabilities
        .output_styles
        .extend(config.output_styles.iter().cloned());

    // Combine top-level memory + bundle-contributed memory for resolution.
    let top_memory = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let bundle_memory = manifest
        .capabilities
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let mut all_memory: Vec<crate::config::Memory> = top_memory
        .iter()
        .chain(bundle_memory.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_memory);

    // Combine host tables: bundle contributions first, top-level wins on collision.
    let mut all_host = manifest.capabilities.host.clone();
    for (k, v) in &config.host {
        all_host.insert(k.clone(), v.clone());
    }

    // Non-project tags for host-level resolution — project-scoped tags must not
    // leak into host plugin/MCP/throttle decisions (#696).
    let host_tags = active.non_project_tags();

    manifest.mcps =
        crate::mcp::resolve::resolve_mcps(&config.mcp, &all_memory, &all_host, &host_tags)
            .context("resolving MCP servers")?;
    manifest.mcps.extend(
        crate::mcp::resolve::resolve_bundle_mcps(&manifest.capabilities.mcp, &host_tags).context(
            "resolving bundle MCP servers \
                 (check mcp: entries in active bundle.yaml files)",
        )?,
    );
    // codebase_memory is inherently project-scoped (each entry indexes one
    // project) and must activate on project tags, unlike the host_tags-only
    // resolution above (#696) — resolved with the full active.tags instead.
    let top_codebase_memory = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    let bundle_codebase_memory = manifest
        .capabilities
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    let mut all_codebase_memory: Vec<crate::config::CodebaseMemory> = top_codebase_memory
        .iter()
        .chain(bundle_codebase_memory.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_codebase_memory);
    if !all_codebase_memory.is_empty() {
        let (project_root, state_dir) = crate::mcp::resolve::codebase_memory_paths()
            .context("resolving codebase_memory paths")?;
        manifest.mcps.extend(
            crate::mcp::resolve::resolve_codebase_memory_entries(
                &all_codebase_memory,
                &active.tags,
                &project_root,
                &state_dir,
            )
            .context("resolving codebase_memory servers")?,
        );
    }
    // Detect cross-source name collisions (global vs bundle).
    {
        let mut seen = std::collections::HashSet::new();
        for m in &manifest.mcps {
            if !seen.insert(m.name.as_str()) {
                anyhow::bail!(
                    "mcp name '{}' declared in both config.mcp and a bundle mcp: — \
                     rename one to avoid ambiguity",
                    m.name
                );
            }
        }
    }

    let cache_root = PathBuf::from(crate::paths::expand_tilde(&config.cache.cache_dir));

    // #920: persist the bundle-only memory/host slice so `hook_run::memory_url`
    // can reuse it instead of redoing this merge on every hook-run invocation.
    // Recomputed here rather than captured earlier (neither `manifest.capabilities
    // .features`/`.host` is mutated between the `bundle_memory`/`all_host`
    // computation above and here — verified: only `manifest.mcps`/`.plugins`/
    // `.marketplaces` are assigned in between). Best-effort — a write failure
    // only costs a cache miss on the hook-run side (it falls back to a live
    // merge), never a correctness issue, so it must not fail `regenerate`/`export`.
    let bundle_memory_slice = manifest
        .capabilities
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    match crate::merge::merge_signature(&config.capabilities, &config.native, &refs) {
        Ok(key) => {
            if let Err(e) = crate::materialize::merge_cache::write(
                &cache_root,
                &key,
                bundle_memory_slice,
                &manifest.capabilities.host,
            ) {
                tracing::warn!("failed to persist bundle-merge cache: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to compute merge signature for cache: {e}"),
    }

    let resolved = crate::plugins::resolve::resolve_plugins(config, &host_tags, true)
        .context("resolving plugins")?;
    manifest.plugins = sync_plugin_payloads(&cache_root, resolved.plugins);
    // `remote_sync = false` means "don't touch the network", not "ignore the
    // clones already on disk". Always resolve `install_location` from existing
    // clones (refresh=false does no git fetch/pull/clone — it only reads local
    // HEAD); just force refresh off when remote sync is disabled. Skipping the
    // call entirely left `install_location = None`, which Claude Code tolerates
    // (reserved marketplaces render as a github source) but opencode/crush do
    // not — they materialize plugin files from the local clone and fail without
    // a path. Mirrors `sync_plugin_payloads`, which already resolves local
    // clones unconditionally.
    let refresh_marketplaces = refresh_marketplaces && config.cache.remote_sync;
    manifest.marketplaces = sync_marketplaces(
        config,
        &cache_root,
        resolved.marketplaces,
        refresh_marketplaces,
    )?;

    // Resolve the active throttle entry (tag intersection, single-active).
    let top_throttle = config
        .features
        .as_ref()
        .map(|f| f.throttle.as_slice())
        .unwrap_or_default();
    let bundle_throttle = manifest
        .capabilities
        .features
        .as_ref()
        .map(|f| f.throttle.as_slice())
        .unwrap_or_default();
    let mut all_throttle: Vec<crate::config::Throttle> = top_throttle
        .iter()
        .chain(bundle_throttle.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_throttle);
    manifest.throttle = crate::throttle::resolve_active_throttle(&all_throttle, &host_tags)
        .context("resolving throttle config")?;

    manifest.session_log = config.session_log_resolved();

    // Fold the root-level `config.features` into the merged manifest. `merge()`
    // above is fed only `config.capabilities`, so a root `features:` block never
    // reaches `manifest.capabilities.features` on its own — and renderers gate on
    // the manifest (slippage fragments, the built-in skill docs, the task-tracker
    // hooks, `mcp_permissions` overrides). Done last so the memory/throttle/
    // codebase_memory resolution above (which reads the pre-fold manifest plus
    // root config directly) isn't double-counted (#987).
    manifest.capabilities.features = fold_root_features(
        config.features.as_ref(),
        manifest.capabilities.features.take(),
    );

    Ok(Some((manifest, cache_root)))
}

/// Sync one marketplace and fill `rm.install_location` + `rm.head`.
/// Returns `Some(rm)` on success, `None` when the marketplace isn't cloned
/// yet and `refresh` is false (warn-and-skip, #282).
fn sync_one_marketplace(
    cache_root: &Path,
    market: &crate::config::Marketplace,
    mut rm: crate::plugins::resolve::ResolvedMarketplace,
    refresh: bool,
) -> anyhow::Result<Option<crate::plugins::resolve::ResolvedMarketplace>> {
    match crate::plugins::cache::sync_marketplace(cache_root, market, refresh) {
        Ok(state) => {
            rm.install_location = Some(state.install_location.to_string_lossy().into_owned());
            rm.head = state.head;
            Ok(Some(rm))
        }
        // (#282) During export (refresh=false), a marketplace that isn't cloned
        // locally should not abort materialization — warn and skip so
        // CLAUDE_CONFIG_DIR can still be emitted. run_plugin_sync (refresh=true)
        // still propagates: an explicit sync that can't reach the remote is a
        // real failure the user needs to see.
        Err(crate::plugins::cache::SyncError::NotCloned { .. }) => {
            eprintln!(
                "warning: marketplace '{}' not yet cloned\n  → plugins from this marketplace \
                 are excluded; run `llmenv plugin-sync` to fetch it",
                rm.name
            );
            Ok(None)
        }
        Err(e) => Err(anyhow::anyhow!("syncing marketplace '{}': {e}", rm.name)),
    }
}

/// Sync each resolved marketplace into the shared cache and fill in its
/// `install_location` + `head`. `refresh` controls whether git sources are
/// network-refreshed (`plugin sync`) or used as-is (`export`).
///
/// Resolved marketplaces not present in `config.marketplace` are built-in
/// injections (e.g. context-mode when `features.context_mode.enabled`). They
/// carry their own source URL and are synced via the same logic as declared ones.
fn sync_marketplaces(
    config: &Config,
    cache_root: &Path,
    resolved: Vec<crate::plugins::resolve::ResolvedMarketplace>,
    refresh: bool,
) -> anyhow::Result<Vec<crate::plugins::resolve::ResolvedMarketplace>> {
    let by_name: std::collections::HashMap<&str, &crate::config::Marketplace> = config
        .marketplace
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    let mut out = Vec::with_capacity(resolved.len());
    for rm in resolved {
        // For declared marketplaces use the config entry; for built-in injected
        // ones (e.g. context-mode) build a transient Marketplace from the
        // resolved source so they are synced rather than silently passed through.
        let transient;
        let market: &crate::config::Marketplace = match by_name.get(rm.name.as_str()) {
            Some(m) => m,
            None => {
                transient = crate::config::Marketplace {
                    name: rm.name.clone(),
                    source: rm.source.clone(),
                };
                &transient
            }
        };
        if let Some(synced) = sync_one_marketplace(cache_root, market, rm, refresh)? {
            out.push(synced);
        }
    }
    Ok(out)
}

/// Look up stable external plugin payload paths for resolved plugins. Non-refreshing
/// (export path): silently skips plugins whose payload hasn't been synced yet, so a
/// missing payload doesn't abort materialization — users run `llmenv plugin-sync` first.
fn sync_plugin_payloads(
    cache_root: &Path,
    plugins: Vec<crate::plugins::resolve::ResolvedPlugin>,
) -> Vec<crate::plugins::resolve::ResolvedPlugin> {
    plugins
        .into_iter()
        .map(|mut p| {
            let mkt_path = crate::plugins::cache::marketplace_path(cache_root, &p.marketplace);
            let entries = match crate::plugins::cache::read_marketplace_plugins(&mkt_path) {
                Ok(entries) => entries,
                Err(e) => {
                    // #893: read_marketplace_plugins now propagates real read
                    // errors (a missing manifest is Ok(empty)), so surface the
                    // cause rather than a fixed plugin-sync hint that won't fix
                    // a permission error or a corrupt manifest.
                    tracing::warn!(
                        "cannot read marketplace manifest for '{}' ({e:#}) — \
                         skipping external plugin '{}'",
                        p.marketplace,
                        p.plugin
                    );
                    return p;
                }
            };
            let Some(entry) = entries.iter().find(|e| e.name == p.plugin) else {
                tracing::warn!(
                    "plugin '{}' not found in marketplace '{}' manifest — \
                     verify plugin name or run `llmenv plugin-sync` to refresh the clone",
                    p.plugin,
                    p.marketplace
                );
                return p;
            };
            if !crate::plugins::cache::is_external_plugin_source(&entry.source) {
                return p;
            }
            match crate::plugins::cache::sync_external_plugin(
                cache_root,
                &p.marketplace,
                &p.plugin,
                &entry.source,
                false,
            ) {
                Ok(state) => {
                    p.install_path = Some(state.install_location.to_string_lossy().into_owned());
                    p.git_commit_sha = state.head;
                }
                Err(crate::plugins::cache::SyncError::NotCloned { .. }) => {
                    eprintln!(
                        "warning: external plugin '{}@{}' not yet fetched\n  \
                         → run `llmenv plugin-sync` to download it",
                        p.plugin, p.marketplace
                    );
                }
                Err(e) => {
                    eprintln!(
                        "warning: external plugin '{}@{}' payload lookup failed: {e}",
                        p.plugin, p.marketplace
                    );
                }
            }
            p
        })
        .collect()
}

/// Fold `root`'s scalars/lists into `merged` (the bundle-merged features),
/// with `root` as the highest-precedence contributor, so its scalars win and
/// its list entries lead.
///
/// The `root` struct is destructured exhaustively on purpose: adding a
/// `Features` field fails to compile here until it's folded, so the manifest
/// can't silently fall back out of sync.
fn fold_root_features(
    root: Option<&crate::config::Features>,
    merged: Option<crate::config::Features>,
) -> Option<crate::config::Features> {
    let Some(root) = root else {
        return merged;
    };
    let crate::config::Features {
        memory,
        throttle,
        codebase_memory,
        context_mode,
        upgrade,
        read_once,
        repeat_detect,
        slippage,
        task_tracker,
        cd_guard,
    } = root.clone();
    let mut out = merged.unwrap_or_default();

    // Lists: root entries lead, then the merged ones, deduped.
    out.memory = concat_dedup(memory, out.memory);
    out.throttle = concat_dedup(throttle, out.throttle);
    out.codebase_memory = concat_dedup(codebase_memory, out.codebase_memory);
    // Scalars: the root value wins when set, else keep the merged one.
    out.context_mode = context_mode.or(out.context_mode);
    out.upgrade = upgrade.or(out.upgrade);
    out.read_once = read_once.or(out.read_once);
    out.repeat_detect = repeat_detect.or(out.repeat_detect);
    out.slippage = slippage.or(out.slippage);
    out.task_tracker = task_tracker.or(out.task_tracker);
    out.cd_guard = cd_guard.or(out.cd_guard);

    Some(out)
}

/// Concatenate `lead` before `rest`, then dedup (first-seen order preserved).
fn concat_dedup<T: PartialEq + Clone>(mut lead: Vec<T>, rest: Vec<T>) -> Vec<T> {
    lead.extend(rest);
    crate::util::dedup(&mut lead);
    lead
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // #987: root `config.features` must reach `manifest.capabilities.features`.
    #[test]
    fn fold_root_features_propagates_root_scalar_into_empty_manifest() {
        let root = crate::config::Features {
            task_tracker: Some(crate::config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = fold_root_features(Some(&root), None).expect("features present");
        assert_eq!(
            out.task_tracker,
            Some(crate::config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            "a root-only feature scalar must land in the manifest"
        );
    }

    #[test]
    fn fold_root_features_root_scalar_wins_over_merged() {
        let root = crate::config::Features {
            task_tracker: Some(crate::config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = crate::config::Features {
            task_tracker: Some(crate::config::TaskTracker {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = fold_root_features(Some(&root), Some(merged)).expect("features present");
        assert_eq!(
            out.task_tracker,
            Some(crate::config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            "root config outranks a bundle-contributed scalar"
        );
    }

    #[test]
    fn fold_root_features_none_root_passes_merged_through() {
        let merged = Some(crate::config::Features {
            slippage: None,
            ..Default::default()
        });
        assert_eq!(fold_root_features(None, merged.clone()), merged);
    }

    #[test]
    fn concat_dedup_leads_with_root_then_dedups() {
        assert_eq!(concat_dedup(vec![1, 2], vec![2, 3]), vec![1, 2, 3]);
    }

    // #281: marketplace sync failure must not silently drop CLAUDE_CONFIG_DIR.

    fn marketplace_config(name: &str, source: &str) -> Config {
        Config {
            marketplace: vec![crate::config::Marketplace {
                name: name.into(),
                source: source.into(),
            }],
            ..Config::default()
        }
    }

    fn resolved_marketplace(name: &str) -> crate::plugins::resolve::ResolvedMarketplace {
        crate::plugins::resolve::ResolvedMarketplace {
            name: name.into(),
            source: String::new(),
            install_location: None,
            head: None,
        }
    }

    #[test]
    fn sync_marketplaces_git_not_cloned_non_fatal_when_not_refreshing() {
        // A git marketplace that isn't cloned locally should be skipped (with a
        // warning) during export (refresh=false) so materialization can continue
        // and CLAUDE_CONFIG_DIR is still emitted. (#282)
        let config = marketplace_config("remote", "https://github.com/example/plugins.git");
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_marketplaces(
            &config,
            tmp.path(),
            vec![resolved_marketplace("remote")],
            false,
        );
        assert!(
            result.is_ok(),
            "git not-cloned during export must be non-fatal"
        );
        assert!(
            result.unwrap().is_empty(),
            "non-cloned marketplace is dropped from output"
        );
    }

    #[test]
    fn sync_marketplaces_path_not_exist_fatal() {
        // A path source that doesn't exist is a configuration error and should
        // fail hard, even during export (refresh=false). (#282)
        let config = marketplace_config("missing", "/nonexistent/path/to/plugins");
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_marketplaces(
            &config,
            tmp.path(),
            vec![resolved_marketplace("missing")],
            false,
        );
        assert!(result.is_err(), "path source not existing must be fatal");
    }

    #[test]
    fn sync_marketplaces_propagates_error_when_refreshing() {
        // An explicit plugin-sync (refresh=true) must still fail hard when a
        // marketplace can't be synced, so the user knows the refresh failed. (#281)
        let config = marketplace_config("missing", "/nonexistent/path/to/plugins");
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_marketplaces(
            &config,
            tmp.path(),
            vec![resolved_marketplace("missing")],
            true,
        );
        assert!(result.is_err(), "refresh=true sync failure must propagate");
    }

    #[test]
    fn sync_marketplaces_succeeds_when_marketplace_available() {
        // A marketplace whose path source exists should succeed in both modes.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("my-market");
        std::fs::create_dir(&src).unwrap();
        let config = marketplace_config("local", &src.to_string_lossy());
        let cache = tempfile::tempdir().unwrap();
        for refresh in [false, true] {
            let result = sync_marketplaces(
                &config,
                cache.path(),
                vec![resolved_marketplace("local")],
                refresh,
            );
            assert!(
                result.is_ok(),
                "available marketplace should succeed (refresh={refresh})"
            );
            let out = result.unwrap();
            assert_eq!(out.len(), 1);
            assert!(
                out[0].install_location.is_some(),
                "install_location filled in"
            );
        }
    }

    #[test]
    fn sync_marketplaces_injected_builtin_is_synced_not_silently_skipped() {
        // Regression test for #490: a resolved marketplace that is NOT in
        // config.marketplace (i.e. the injected context-mode built-in) must be
        // synced — not silently passed through with install_location=None.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("context-mode");
        std::fs::create_dir(&src).unwrap();

        // config.marketplace is empty — simulates user having only
        // features.context_mode.enabled: true without a manual marketplace entry.
        let config = Config {
            marketplace: vec![],
            ..Config::default()
        };
        let cache = tempfile::tempdir().unwrap();

        // The resolved entry carries the source (as inject_context_mode sets it).
        let rm = crate::plugins::resolve::ResolvedMarketplace {
            name: "context-mode".into(),
            source: src.to_string_lossy().into_owned(),
            install_location: None,
            head: None,
        };

        let result = sync_marketplaces(&config, cache.path(), vec![rm], false);
        assert!(
            result.is_ok(),
            "injected built-in should sync without error"
        );
        let out = result.unwrap();
        assert_eq!(out.len(), 1, "injected marketplace must appear in output");
        assert!(
            out[0].install_location.is_some(),
            "install_location must be filled in for injected built-in (was None before fix)"
        );
    }
}
