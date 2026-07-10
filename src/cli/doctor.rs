use crate::config::{Bundle, Config};
use crate::paths;
use anyhow::Context;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

pub(super) fn run_doctor_token_efficiency(
    config: &Config,
    use_color: bool,
    pass: &str,
    warn: &str,
) {
    let info = super::doctor_info(use_color);
    eprintln!();
    eprintln!("Token-efficiency checks:");

    match std::env::var("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE") {
        Ok(val) => match val.parse::<u32>() {
            Ok(pct) if pct <= 70 => eprintln!("{pass} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct}"),
            Ok(pct) => eprintln!(
                "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct} (recommend ≤70 for PreCompact cleanup)"
            ),
            Err(_) => {
                eprintln!("{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE has invalid (non-numeric) value")
            }
        },
        Err(_) => eprintln!(
            "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE not set (recommend 50 for PreCompact headroom)"
        ),
    }

    match std::env::var("BASH_MAX_OUTPUT_LENGTH").map(|v| v.parse::<u64>()) {
        Ok(Ok(n)) => eprintln!("{pass} BASH_MAX_OUTPUT_LENGTH={n}"),
        Ok(Err(_)) => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH has invalid (non-numeric) value"),
        Err(_) => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH not set (recommend 10000)"),
    }

    match std::env::var("MAX_MCP_OUTPUT_TOKENS").map(|v| v.parse::<u64>()) {
        Ok(Ok(n)) => eprintln!("{pass} MAX_MCP_OUTPUT_TOKENS={n}"),
        Ok(Err(_)) => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS has invalid (non-numeric) value"),
        Err(_) => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS not set (recommend 10000)"),
    }

    match std::env::var("ENABLE_PROMPT_CACHING_1H") {
        Ok(val) if val.eq_ignore_ascii_case("true") || val == "1" => {
            eprintln!("{pass} ENABLE_PROMPT_CACHING_1H=true (1h cache TTL enabled)")
        }
        Ok(_) => eprintln!("{warn} ENABLE_PROMPT_CACHING_1H has unexpected value (recommend true)"),
        Err(_) => {
            eprintln!("{warn} ENABLE_PROMPT_CACHING_1H not set (recommend true for 1h cache reuse)")
        }
    }

    match std::env::var("CLAUDE_CODE_SUBAGENT_MODEL") {
        Ok(_) => eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL is set"),
        Err(_) => {
            eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL not set (default: claude-sonnet-4-6)")
        }
    }

    let has_context_mode = config.mcp.iter().any(|m| m.name.contains("context-mode"));
    if has_context_mode {
        eprintln!("{pass} context-mode MCP server is configured");
    } else {
        eprintln!("{warn} context-mode MCP not configured (load-bearing for token efficiency)");
        eprintln!("{warn}   → Install context-mode plugin and add to mcp: section in config.yaml");
    }
}

/// Returns bundle names whose directory does not exist under `bundles_dir`.
pub(super) fn bundles_with_missing_dirs<'a>(
    bundles: &'a [Bundle],
    bundles_dir: &Path,
) -> Vec<&'a str> {
    bundles
        .iter()
        .filter(|b| !bundles_dir.join(&b.name).exists())
        .map(|b| b.name.as_str())
        .collect()
}

/// Returns marketplace names defined in `config` that no plugin collection references.
pub(super) fn unused_marketplaces(config: &Config) -> Vec<&str> {
    use crate::config::split_plugin_ref;
    let referenced: HashSet<&str> = config
        .plugin_collection
        .iter()
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p))
        .map(|(m, _)| m)
        .collect();
    config
        .marketplace
        .iter()
        .filter(|m| !referenced.contains(m.name.as_str()))
        .map(|m| m.name.as_str())
        .collect()
}

/// Returns `native_permissions` keys that don't match any configured MCP server
/// or known engine adapter name. The ICM MCP server (`"icm"`) is always present.
pub(super) fn orphan_native_permission_keys(config: &Config) -> Vec<&str> {
    let known_mcps: HashSet<&str> = config
        .mcp
        .iter()
        .map(|m| m.name.as_str())
        .chain(std::iter::once("icm"))
        .collect();
    const ENGINE_NAMES: &[&str] = &["claude_code"];
    config
        .capabilities
        .native_permissions
        .keys()
        .filter(|k| !known_mcps.contains(k.as_str()) && !ENGINE_NAMES.contains(&k.as_str()))
        .map(|k| k.as_str())
        .collect()
}

pub(super) fn run_doctor(gc: bool, all: bool, use_color: bool) -> anyhow::Result<()> {
    let pass = super::doctor_pass(use_color);
    let warn = super::doctor_warning(use_color);
    let info = super::doctor_info(use_color);

    eprintln!("Running llmenv doctor...");

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    eprintln!("{pass} Configuration loaded from {}", config_path.display());

    // Check that config parses
    eprintln!("{pass} Config is valid YAML");

    // Structural validation: bundle directories, marketplace references, permission grants
    let config_dir = paths::config_dir()?;
    let bundles_dir = config_dir.join("bundles");

    for name in bundles_with_missing_dirs(&config.bundle, &bundles_dir) {
        eprintln!(
            "{info} Bundle '{name}' declared but directory does not exist at {}",
            bundles_dir.join(name).display(),
        );
    }

    for name in unused_marketplaces(&config) {
        eprintln!(
            "{warn} Marketplace '{name}' is defined but not referenced by any plugin collection",
        );
    }

    for key in orphan_native_permission_keys(&config) {
        eprintln!(
            "{warn} native_permissions key '{key}' does not match any configured MCP server or plugin",
        );
    }

    // Check cache directory is writable
    let cache_dir = super::expand_tilde(&config.cache.cache_dir)?;
    std::fs::create_dir_all(&cache_dir).context("cache directory not writable")?;
    eprintln!(
        "{pass} Cache directory is writable: {}",
        cache_dir.display()
    );

    // Report the active cache layout so `doctor` explains the folder shape on disk.
    match config.cache.hashing {
        crate::config::HashingMode::Loose => {
            eprintln!("{pass} Cache hashing: loose (folder: <shape>)");
        }
        crate::config::HashingMode::Normal => {
            eprintln!(
                "{pass} Cache hashing: normal (folder: {}/<shape>)",
                crate::materialize::cache::version_mm()
            );
        }
        crate::config::HashingMode::Strict => {
            eprintln!("{pass} Cache hashing: strict (content-addressed folders)");
        }
    }

    // Check for version skew across all registered adapters
    let skew_relevant = !matches!(config.cache.hashing, crate::config::HashingMode::Loose);
    if skew_relevant {
        for adapter in crate::adapter::registered_adapters() {
            let adapter_cache = cache_dir.join(adapter.name());
            if let Ok(entries) = std::fs::read_dir(&adapter_cache) {
                let mut cached_versions: Vec<String> = Vec::new();
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if dir_name.ends_with(".tmp") {
                        continue;
                    }
                    let version = match dir_name.rsplit_once('-') {
                        Some((prefix, tail)) if super::is_content_hash(tail) => prefix.to_string(),
                        _ => dir_name.to_string(),
                    };
                    cached_versions.push(version);
                }
                cached_versions.sort();
                cached_versions.dedup();
                let version_folder = crate::materialize::cache::version_mm();
                let current_built = |v: &String| v == super::VERSION_TAG || *v == version_folder;
                if !cached_versions.is_empty() {
                    let cached_versions_str = cached_versions.join(", ");
                    if !cached_versions.iter().any(current_built) {
                        eprintln!(
                            "{warn} {} version skew detected: running llmenv {} but cache has versions [{}]",
                            adapter.name(),
                            super::VERSION_TAG,
                            cached_versions_str
                        );
                        eprintln!("{warn}   → Fix: cargo install --path . --force");
                    }
                }
            }
        }
    }

    // Check git remote is reachable
    if super::is_git_repo(&config_dir) {
        match super::check_git_remote(&config_dir) {
            Ok(remote) => {
                let safe_url = crate::git::sanitize_git_url(&remote);
                eprintln!("{pass} Git remote reachable: {}", safe_url);
            }
            Err(e) => eprintln!("{warn} Git remote check failed: {}", e),
        }
    } else {
        eprintln!("{warn} Config directory is not a git repo");
    }

    if all {
        // Orphan detection
        let env = crate::scope::matcher::Env::detect();
        let active = crate::scope::evaluate(&config, &env);
        let mut emitted = super::all_emitted_tags(&config);
        emitted.extend(active.tags.iter().cloned());
        let consumed = super::all_consumed_tags(&config);
        let marker_enabled = super::marker_enabled_bundle_names(&active);

        let mut orphan_count: usize = 0;
        for s in &config.scope.network {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope network:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }
        for s in &config.scope.host {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope host:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }
        for s in &config.scope.user {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope user:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }

        let configured_bundle_names: std::collections::HashSet<&str> =
            config.bundle.iter().map(|b| b.name.as_str()).collect();
        for scope in &active.scopes {
            if scope.kind != "project" {
                continue;
            }
            for field in &scope.unknown_fields {
                eprintln!("{warn} unknown field in .llmenv.yaml: {field}");
                orphan_count += 1;
            }
            for bundle_name in &scope.enable_bundles {
                if !configured_bundle_names.contains(bundle_name.as_str()) {
                    eprintln!(
                        "{warn} .llmenv.yaml enable_bundles references unknown bundle: {bundle_name}"
                    );
                    orphan_count += 1;
                }
            }
        }

        for b in &config.bundle {
            let has_emitted_tag = b.when.iter().any(|t| emitted.contains(t));
            let looks_marker = super::looks_marker_driven(&b.name, b);
            if !has_emitted_tag && !marker_enabled.contains(&b.name) && !looks_marker {
                eprintln!("{warn} orphan bundle {}: no scope emits its tags", b.name);
                orphan_count += 1;
            }
        }

        for m in &config.mcp {
            let has_emitted_tag = m.when.iter().any(|t| emitted.contains(t));
            let looks_marker = m.when.iter().any(|t| super::tag_looks_marker_sourced(t));
            if !has_emitted_tag && !looks_marker {
                eprintln!("{warn} orphan mcp {}: no scope emits its tags", m.name);
                orphan_count += 1;
            }
        }

        // Build merged host table for server_host checks
        let doctor_firing: Vec<_> = {
            let manually: BTreeSet<&str> = active
                .scopes
                .iter()
                .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
                .collect();
            config
                .bundle
                .iter()
                .filter(|b| {
                    b.when.iter().any(|bt| active.tags.contains(bt))
                        || manually.contains(b.name.as_str())
                })
                .collect()
        };

        let doctor_bundle_caps = {
            let refs = super::build_bundle_refs(&config_dir, &active, &doctor_firing);
            if refs.is_empty() {
                crate::config::Capabilities::default()
            } else {
                crate::merge::merge(&config.capabilities, &config.native, &refs)
                    .context("failed to merge bundle capabilities for orphan check")?
                    .capabilities
            }
        };

        let mut merged_host_for_doctor = doctor_bundle_caps.host.clone();
        for (k, v) in &config.host {
            merged_host_for_doctor.insert(k.clone(), v.clone());
        }

        // Check top-level memory entries
        if let Some(features) = &config.features {
            for mem in &features.memory {
                let has_emitted_tag = mem.when.iter().any(|t| emitted.contains(t));
                if !has_emitted_tag {
                    eprintln!(
                        "{warn} orphan memory (server_host '{}'): no scope emits its tags",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
                if !merged_host_for_doctor.contains_key(&mem.server_host) {
                    eprintln!(
                        "{warn} memory: server_host '{}' has no entry in the host: table",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
            }
        }

        // Check bundle-contributed memory entries
        if let Some(features) = &doctor_bundle_caps.features {
            for mem in &features.memory {
                let has_emitted_tag = mem.when.iter().any(|t| emitted.contains(t));
                if !has_emitted_tag {
                    eprintln!(
                        "{warn} orphan bundle memory (server_host '{}'): no scope emits its tags",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
                if !merged_host_for_doctor.contains_key(&mem.server_host) {
                    eprintln!(
                        "{warn} bundle memory: server_host '{}' has no entry in host: table",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
            }
        }

        // Plugin orphans
        {
            use crate::config::split_plugin_ref;

            let mut referenceable: HashSet<&str> = HashSet::new();
            for c in &config.plugin_collection {
                let selectable = c.when.iter().any(|t| emitted.contains(t));
                if !selectable {
                    eprintln!(
                        "{warn} orphan plugin-collection {}: no scope emits its tags",
                        c.name
                    );
                    orphan_count += 1;
                }
                if selectable {
                    referenceable.extend(
                        c.plugins
                            .iter()
                            .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m)),
                    );
                }
            }
            for m in &config.marketplace {
                if !referenceable.contains(m.name.as_str()) {
                    eprintln!(
                        "{warn} orphan marketplace {}: no selectable plugin references it",
                        m.name
                    );
                    orphan_count += 1;
                }
            }
        }

        // Tag orphans
        let mut tag_universe: HashSet<String> = HashSet::new();
        tag_universe.extend(emitted.iter().cloned());
        tag_universe.extend(consumed.iter().cloned());
        tag_universe.extend(active.tags.iter().cloned());
        let mut tag_orphans: Vec<String> = tag_universe
            .into_iter()
            .filter(|t| {
                let emitted_anywhere = emitted.contains(t)
                    || active.tags.contains(t)
                    || super::tag_looks_marker_sourced(t);
                let consumed_anywhere = consumed.contains(t);
                !(emitted_anywhere && consumed_anywhere)
            })
            .collect();
        tag_orphans.sort();
        for t in &tag_orphans {
            let emitted_anywhere = emitted.contains(t)
                || active.tags.contains(t)
                || super::tag_looks_marker_sourced(t);
            let reason = if !emitted_anywhere {
                "no scope emits it"
            } else {
                "no bundle consumes it"
            };
            eprintln!("{warn} orphan tag {}: {}", t, reason);
            orphan_count += 1;
        }

        if orphan_count == 0 {
            eprintln!("{pass} No orphan scopes/tags/bundles/plugins");
        } else {
            eprintln!("{warn} Found {} orphan item(s)", orphan_count);
        }
    } // end if all

    // Lint for ${CLAUDE_PLUGIN_ROOT} in non-plugin hooks
    for hook in &config.capabilities.hooks {
        if let Some(cmd) = &hook.handler.command
            && cmd.contains("${CLAUDE_PLUGIN_ROOT}")
        {
            eprintln!(
                "{warn} Hook command references ${{CLAUDE_PLUGIN_ROOT}} but runs in top-level settings.json: {}",
                cmd
            );
            eprintln!(
                "{warn}   → ${{CLAUDE_PLUGIN_ROOT}} only works in plugin-scoped hooks/hooks.json files"
            );
            eprintln!("{warn}   → Move or rewrite this hook in your config or bundle YAML");
        }
    }

    run_doctor_token_efficiency(&config, use_color, &pass, &warn);

    eprintln!("{pass} Doctor check complete.");

    if gc {
        eprintln!("Running garbage collection...");
        match std::fs::metadata(&cache_dir) {
            Ok(meta) => {
                if meta.permissions().readonly() {
                    eprintln!("{warn} GC failed: cache directory is read-only");
                } else {
                    let cache_retention_hours = config.cache.cache_retention_hours.unwrap_or(168);
                    let retention = std::time::Duration::from_secs(cache_retention_hours * 3600);
                    match crate::materialize::cache::gc(&cache_dir, retention) {
                        Ok(report) => {
                            eprintln!(
                                "{pass} GC complete: removed {} entries, kept {}",
                                report.removed.len(),
                                report.kept
                            );
                        }
                        Err(e) => eprintln!("{warn} GC failed: {}", e),
                    }
                }
            }
            Err(e) => eprintln!("{warn} GC failed to stat cache directory: {}", e),
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::needless_pass_by_value
)]
mod tests {
    use super::*;
    use crate::config::{
        Bundle, Capabilities, Marketplace, McpServer, McpTransport, NativePermissionRules,
        PluginCollection,
    };
    use std::collections::BTreeMap;

    // -- bundles_with_missing_dirs --

    #[test]
    fn bundles_missing_none_when_all_dirs_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("home")).unwrap();
        std::fs::create_dir_all(bundles_dir.join("work")).unwrap();

        let bundles = vec![
            Bundle {
                name: "home".into(),
                when: vec!["local".into()],
            },
            Bundle {
                name: "work".into(),
                when: vec!["office".into()],
            },
        ];
        let missing = bundles_with_missing_dirs(&bundles, &bundles_dir);
        assert!(missing.is_empty(), "expected empty: {missing:?}");
    }

    #[test]
    fn bundles_missing_reports_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("existing")).unwrap();

        let bundles = vec![
            Bundle {
                name: "existing".into(),
                when: vec!["x".into()],
            },
            Bundle {
                name: "missing".into(),
                when: vec!["y".into()],
            },
        ];
        let mut missing = bundles_with_missing_dirs(&bundles, &bundles_dir);
        missing.sort_unstable();
        assert_eq!(missing, vec!["missing"]);
    }

    // -- unused_marketplaces --

    #[test]
    fn unused_marketplaces_none_when_all_referenced() {
        let config = Config {
            marketplace: vec![Marketplace {
                name: "official".into(),
                source: "https://example.com".into(),
            }],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: vec!["official:some-plugin".into()],
            }],
            ..Config::default()
        };
        let unused = unused_marketplaces(&config);
        assert!(unused.is_empty(), "expected empty: {unused:?}");
    }

    #[test]
    fn unused_marketplaces_reports_unreferenced() {
        let config = Config {
            marketplace: vec![
                Marketplace {
                    name: "used".into(),
                    source: "https://a.com".into(),
                },
                Marketplace {
                    name: "unused".into(),
                    source: "https://b.com".into(),
                },
            ],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: vec!["used:plugin-a".into()],
            }],
            ..Config::default()
        };
        let mut unused = unused_marketplaces(&config);
        unused.sort_unstable();
        assert_eq!(unused, vec!["unused"]);
    }

    // -- orphan_native_permission_keys --

    #[test]
    fn orphan_permissions_none_for_known_engine() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "claude_code".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_permissions_accepts_icm() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "icm".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_permissions_accepts_configured_mcp() {
        let config = Config {
            mcp: vec![McpServer {
                name: "my-server".into(),
                when: vec![],
                transport: McpTransport::Stdio,
                command: Some("echo".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            }],
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "my-server".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_permissions_reports_unknown_key() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "mcp__unknown-server".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert_eq!(orphans, vec!["mcp__unknown-server"]);
    }

    #[test]
    fn orphan_permissions_empty_no_permissions() {
        let config = Config::default();
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }
}
