//! Resolves configured plugin collections into concrete, render-ready entries
//! for the active host.
//!
//! Selection mirrors bundles and MCP servers: a [`crate::config::PluginCollection`]
//! is selected when any of its `tags` intersect the active scope tag set. The
//! union of all selected collections' plugins (deduplicated, in stable order) is
//! what an adapter wires up. Each plugin carries its originating marketplace and
//! the set of referenced marketplaces is resolved alongside so adapters can emit
//! both the plugin list and the marketplace registry.

use std::collections::{BTreeSet, HashSet};

use crate::config::{Config, Marketplace, split_plugin_ref};

/// A fully resolved plugin ready for an adapter to render. Carries the
/// originating marketplace and plugin name split out from the
/// `<marketplace>:<plugin>` config form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedPlugin {
    /// Marketplace name (left half of the config `marketplace:plugin` string).
    pub marketplace: String,
    /// Plugin name (right half).
    pub plugin: String,
    /// Name of the collection that first selected this plugin, for provenance
    /// in `plugin ls`.
    pub collection: String,
    /// Absolute on-disk path to the plugin payload for externally-sourced plugins
    /// (those whose `source` in marketplace.json is a git URL, not a relative path).
    /// `None` for first-party plugins (payload lives inside the marketplace clone).
    /// Filled in after `sync_plugin_payloads` runs; resolution leaves it empty.
    pub install_path: Option<String>,
    /// Full git commit SHA of the installed payload. `None` for first-party plugins.
    pub git_commit_sha: Option<String>,
}

/// A marketplace referenced by at least one selected plugin, ready to render
/// into an adapter's marketplace registry.
///
/// `install_location` and `head` are filled in after the marketplace is synced
/// into the cache (see [`crate::plugins::cache`]); resolution leaves them empty
/// because it does no I/O. They participate in the materialized scope hash so a
/// marketplace update re-renders the scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedMarketplace {
    pub name: String,
    pub source: String,
    /// Absolute on-disk path to load the marketplace from. Empty until synced.
    pub install_location: Option<String>,
    /// Content token (git HEAD sha) of the synced marketplace. `None` for local
    /// path sources or before syncing.
    pub head: Option<String>,
}

/// The render-ready result of resolving plugins for the active host: the
/// deduplicated plugin list plus the marketplaces those plugins reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPlugins {
    /// Selected plugins, deduplicated by `(marketplace, plugin)`, in stable
    /// order (collection declaration order, then plugin declaration order).
    pub plugins: Vec<ResolvedPlugin>,
    /// Marketplaces referenced by `plugins`, in declaration order, deduplicated.
    pub marketplaces: Vec<ResolvedMarketplace>,
}

/// Errors raised while resolving plugin config for the active host.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error(
        "plugin-collection '{collection}': invalid plugin '{plugin}' (must be '<marketplace>:<plugin>')"
    )]
    InvalidPluginRef { collection: String, plugin: String },
    #[error(
        "plugin-collection '{collection}': plugin '{plugin}' references unknown marketplace '{marketplace}'"
    )]
    UnknownMarketplace {
        collection: String,
        plugin: String,
        marketplace: String,
    },
}

/// Inject the built-in context-mode marketplace + plugin into the resolved set
/// when `features.context_mode.enabled` (#490). Mutates `plugins`/`seen_plugin`/
/// `referenced` in place; returns whether the synthetic marketplace must be
/// appended (true only when the user did not declare a `context-mode` marketplace).
/// Warns when the user also declared the plugin manually (redundant).
fn inject_context_mode(
    config: &Config,
    plugins: &mut Vec<ResolvedPlugin>,
    seen_plugin: &mut HashSet<(String, String)>,
    referenced: &mut HashSet<String>,
) -> bool {
    // Built-in context-mode feature (#490): inject the canonical marketplace +
    // plugin when enabled, unless the user already declared it (user wins on
    // source). context-mode is a *plugin* (its hooks need ${CLAUDE_PLUGIN_ROOT}),
    // so it rides the normal plugin path — not ICM's remote-MCP mechanism.
    let cm_enabled = config
        .features
        .as_ref()
        .and_then(|f| f.context_mode.as_ref())
        .is_some_and(|c| c.enabled);
    if !cm_enabled {
        return false;
    }
    let key = (
        crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
        crate::config::CONTEXT_MODE_PLUGIN.to_string(),
    );
    if seen_plugin.insert(key) {
        referenced.insert(crate::config::CONTEXT_MODE_MARKETPLACE.to_string());
        plugins.push(ResolvedPlugin {
            marketplace: crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
            plugin: crate::config::CONTEXT_MODE_PLUGIN.to_string(),
            collection: "context_mode (built-in)".to_string(),
            install_path: None,
            git_commit_sha: None,
        });
    } else {
        // The user manually declared context-mode:context-mode in a
        // plugin-collection AND enabled features.context_mode. The built-in
        // already wires it — the manual entry is redundant. Warn so the user
        // can drop it (harmless, but confusing config drift otherwise).
        tracing::warn!(
            "features.context_mode is enabled and you also declared \
             'context-mode:context-mode' in a plugin-collection — the \
             built-in feature wires context-mode automatically, so the manual \
             plugin-collection entry is redundant and can be removed."
        );
    }
    // The built-in marketplace is emitted from config.marketplace below only
    // if the user declared it. If they didn't, we must add it ourselves.
    !config
        .marketplace
        .iter()
        .any(|m| m.name == crate::config::CONTEXT_MODE_MARKETPLACE)
}

/// Select and resolve all plugin collections for the active host.
///
/// `active_tags` is the union of tags emitted by matching scopes. Collections
/// are visited in declaration order; the plugins of each selected collection are
/// flattened into the output. A plugin appearing in more than one selected
/// collection is rendered once (first occurrence wins for provenance). The set
/// of marketplaces referenced by the resolved plugins is collected in
/// marketplace declaration order.
///
/// # Errors
/// Returns the first [`ResolveError`]: a malformed `marketplace:plugin` string,
/// or a plugin referencing a marketplace not declared at the top level.
/// (Validation catches these too, but resolution must not silently emit garbage
/// when called on a config that bypassed validation.)
pub fn resolve_plugins(
    config: &Config,
    active_tags: &BTreeSet<String>,
) -> Result<ResolvedPlugins, ResolveError> {
    let by_name: std::collections::HashMap<&str, &Marketplace> = config
        .marketplace
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();

    let mut plugins: Vec<ResolvedPlugin> = Vec::new();
    let mut seen_plugin: HashSet<(String, String)> = HashSet::new();
    let mut referenced: HashSet<String> = HashSet::new();

    for collection in &config.plugin_collection {
        if !collection.when.iter().any(|t| active_tags.contains(t)) {
            continue;
        }
        for plugin in &collection.plugins {
            let Some((marketplace, name)) = split_plugin_ref(plugin) else {
                return Err(ResolveError::InvalidPluginRef {
                    collection: collection.name.clone(),
                    plugin: plugin.clone(),
                });
            };
            if !by_name.contains_key(marketplace) {
                return Err(ResolveError::UnknownMarketplace {
                    collection: collection.name.clone(),
                    plugin: plugin.clone(),
                    marketplace: marketplace.to_string(),
                });
            }
            let key = (marketplace.to_string(), name.to_string());
            if !seen_plugin.insert(key) {
                continue;
            }
            referenced.insert(marketplace.to_string());
            plugins.push(ResolvedPlugin {
                marketplace: marketplace.to_string(),
                plugin: name.to_string(),
                collection: collection.name.clone(),
                install_path: None,
                git_commit_sha: None,
            });
        }
    }

    let inject_builtin_marketplace =
        inject_context_mode(config, &mut plugins, &mut seen_plugin, &mut referenced);

    // Emit referenced marketplaces in config declaration order so output is
    // stable and diff-friendly regardless of plugin discovery order.
    let mut marketplaces: Vec<ResolvedMarketplace> = config
        .marketplace
        .iter()
        .filter(|m| referenced.contains(&m.name))
        .map(|m| ResolvedMarketplace {
            name: m.name.clone(),
            source: m.source.clone(),
            install_location: None,
            head: None,
        })
        .collect();
    if inject_builtin_marketplace {
        marketplaces.push(ResolvedMarketplace {
            name: crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
            source: crate::config::CONTEXT_MODE_SOURCE.to_string(),
            install_location: None,
            head: None,
        });
    }

    Ok(ResolvedPlugins {
        plugins,
        marketplaces,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::{Marketplace, PluginCollection};

    fn tags(ts: &[&str]) -> BTreeSet<String> {
        ts.iter().map(|s| (*s).to_string()).collect()
    }

    fn mkt(name: &str) -> Marketplace {
        Marketplace {
            name: name.into(),
            source: format!("https://github.com/example/{name}"),
        }
    }

    fn collection(name: &str, tags: &[&str], plugins: &[&str]) -> PluginCollection {
        PluginCollection {
            name: name.into(),
            when: tags.iter().map(|s| (*s).into()).collect(),
            plugins: plugins.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn config_with(marketplaces: Vec<Marketplace>, collections: Vec<PluginCollection>) -> Config {
        Config {
            marketplace: marketplaces,
            plugin_collection: collections,
            ..Config::default()
        }
    }

    #[test]
    fn selects_only_collections_with_intersecting_tags() {
        let cfg = config_with(
            vec![mkt("superpowers"), mkt("dev-commons")],
            vec![
                collection("core", &["user-x"], &["superpowers:caveman"]),
                collection("rust", &["rust"], &["dev-commons:rust-tooling"]),
            ],
        );
        let resolved = resolve_plugins(&cfg, &tags(&["user-x"])).unwrap();
        assert_eq!(resolved.plugins.len(), 1);
        assert_eq!(resolved.plugins[0].marketplace, "superpowers");
        assert_eq!(resolved.plugins[0].plugin, "caveman");
        assert_eq!(resolved.marketplaces.len(), 1);
        assert_eq!(resolved.marketplaces[0].name, "superpowers");
    }

    #[test]
    fn unions_plugins_across_selected_collections() {
        let cfg = config_with(
            vec![mkt("superpowers"), mkt("dev-commons")],
            vec![
                collection("core", &["t"], &["superpowers:caveman", "superpowers:sp"]),
                collection("extra", &["t"], &["dev-commons:nbl-dev"]),
            ],
        );
        let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
        assert_eq!(resolved.plugins.len(), 3);
        assert_eq!(resolved.marketplaces.len(), 2);
    }

    #[test]
    fn dedupes_plugin_appearing_in_two_collections() {
        let cfg = config_with(
            vec![mkt("superpowers")],
            vec![
                collection("a", &["t"], &["superpowers:caveman"]),
                collection("b", &["t"], &["superpowers:caveman"]),
            ],
        );
        let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
        assert_eq!(resolved.plugins.len(), 1);
        // First collection wins for provenance.
        assert_eq!(resolved.plugins[0].collection, "a");
    }

    #[test]
    fn marketplaces_emitted_in_declaration_order() {
        let cfg = config_with(
            vec![mkt("zeta"), mkt("alpha")],
            vec![collection("c", &["t"], &["alpha:one", "zeta:two"])],
        );
        let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
        // Declaration order (zeta then alpha), not plugin-reference order.
        assert_eq!(resolved.marketplaces[0].name, "zeta");
        assert_eq!(resolved.marketplaces[1].name, "alpha");
    }

    #[test]
    fn unreferenced_marketplace_not_emitted() {
        let cfg = config_with(
            vec![mkt("used"), mkt("unused")],
            vec![collection("c", &["t"], &["used:p"])],
        );
        let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
        assert_eq!(resolved.marketplaces.len(), 1);
        assert_eq!(resolved.marketplaces[0].name, "used");
    }

    #[test]
    fn malformed_plugin_ref_errors() {
        let cfg = config_with(
            vec![mkt("m")],
            vec![collection("c", &["t"], &["noseparator"])],
        );
        let err = resolve_plugins(&cfg, &tags(&["t"])).unwrap_err();
        assert!(matches!(err, ResolveError::InvalidPluginRef { .. }));
    }

    #[test]
    fn unknown_marketplace_errors() {
        let cfg = config_with(
            vec![mkt("known")],
            vec![collection("c", &["t"], &["ghost:p"])],
        );
        let err = resolve_plugins(&cfg, &tags(&["t"])).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnknownMarketplace {
                collection: "c".into(),
                plugin: "ghost:p".into(),
                marketplace: "ghost".into(),
            }
        );
    }

    #[test]
    fn no_active_tags_resolves_empty() {
        let cfg = config_with(vec![mkt("m")], vec![collection("c", &["t"], &["m:p"])]);
        let resolved = resolve_plugins(&cfg, &tags(&["other"])).unwrap();
        assert!(resolved.plugins.is_empty());
        assert!(resolved.marketplaces.is_empty());
    }

    #[test]
    fn context_mode_feature_injects_plugin_and_marketplace() {
        let cfg = Config {
            features: Some(crate::config::Features {
                context_mode: Some(crate::config::ContextMode { enabled: true }),
                ..Default::default()
            }),
            ..Config::default()
        };
        let resolved = resolve_plugins(&cfg, &tags(&[])).unwrap();
        assert!(
            resolved
                .plugins
                .iter()
                .any(|p| p.marketplace == "context-mode" && p.plugin == "context-mode")
        );
        assert!(
            resolved
                .marketplaces
                .iter()
                .any(|m| m.name == "context-mode"
                    && m.source == "https://github.com/mksglu/context-mode")
        );
    }

    #[test]
    fn context_mode_disabled_injects_nothing() {
        let cfg = Config::default();
        let resolved = resolve_plugins(&cfg, &tags(&[])).unwrap();
        assert!(
            !resolved
                .plugins
                .iter()
                .any(|p| p.marketplace == "context-mode")
        );
    }

    #[test]
    fn context_mode_dedups_user_declared() {
        // User declares context-mode via a marketplace + collection AND enables the
        // feature: exactly one plugin entry, user's source preserved.
        let cfg = Config {
            marketplace: vec![Marketplace {
                name: "context-mode".into(),
                source: "https://github.com/myfork/context-mode".into(),
            }],
            plugin_collection: vec![crate::config::PluginCollection {
                name: "core".into(),
                when: vec!["t".into()],
                plugins: vec!["context-mode:context-mode".into()],
            }],
            features: Some(crate::config::Features {
                context_mode: Some(crate::config::ContextMode { enabled: true }),
                ..Default::default()
            }),
            ..Config::default()
        };
        let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
        let cm: Vec<_> = resolved
            .plugins
            .iter()
            .filter(|p| p.marketplace == "context-mode")
            .collect();
        assert_eq!(cm.len(), 1, "no duplicate plugin entry");
        let mk: Vec<_> = resolved
            .marketplaces
            .iter()
            .filter(|m| m.name == "context-mode")
            .collect();
        assert_eq!(mk.len(), 1);
        assert_eq!(
            mk[0].source, "https://github.com/myfork/context-mode",
            "user-declared source wins"
        );
    }

    #[test]
    fn context_mode_user_declared_triggers_dedup_branch() {
        // Same setup as the dedup test: feature on + user-declared plugin. The
        // warn fires on the dedup branch; we assert the branch was taken (one entry)
        // which is the same observable the warn guards.
        let cfg = Config {
            marketplace: vec![Marketplace {
                name: "context-mode".into(),
                source: "https://github.com/mksglu/context-mode".into(),
            }],
            plugin_collection: vec![crate::config::PluginCollection {
                name: "core".into(),
                when: vec!["t".into()],
                plugins: vec!["context-mode:context-mode".into()],
            }],
            features: Some(crate::config::Features {
                context_mode: Some(crate::config::ContextMode { enabled: true }),
                ..Default::default()
            }),
            ..Config::default()
        };
        let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
        assert_eq!(
            resolved
                .plugins
                .iter()
                .filter(|p| p.marketplace == "context-mode")
                .count(),
            1
        );
    }

    #[test]
    fn context_mode_user_marketplace_only_preserves_source() {
        // User declares the context-mode marketplace (fork source) but NO
        // plugin-collection entry, feature enabled: plugin is injected AND the
        // user's marketplace source is preserved (exactly one of each).
        let cfg = Config {
            marketplace: vec![Marketplace {
                name: "context-mode".into(),
                source: "https://github.com/myfork/context-mode".into(),
            }],
            features: Some(crate::config::Features {
                context_mode: Some(crate::config::ContextMode { enabled: true }),
                ..Default::default()
            }),
            ..Config::default()
        };
        let resolved = resolve_plugins(&cfg, &tags(&[])).unwrap();
        assert_eq!(
            resolved
                .plugins
                .iter()
                .filter(|p| p.marketplace == "context-mode")
                .count(),
            1,
            "plugin injected exactly once"
        );
        let mk: Vec<_> = resolved
            .marketplaces
            .iter()
            .filter(|m| m.name == "context-mode")
            .collect();
        assert_eq!(mk.len(), 1, "exactly one marketplace entry");
        assert_eq!(
            mk[0].source, "https://github.com/myfork/context-mode",
            "user-declared source preserved"
        );
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn arb_collection(idx: usize) -> impl Strategy<Value = PluginCollection> {
            (
                prop::collection::vec("[a-z]{1,4}", 0..3),
                prop::collection::vec("[a-z]{1,4}", 0..3),
            )
                .prop_map(move |(ts, names)| PluginCollection {
                    name: format!("col-{idx}"),
                    when: ts,
                    // All plugins reference the single marketplace "m" so
                    // resolution never errors and we reason purely about
                    // selection + dedup.
                    plugins: names.into_iter().map(|n| format!("m:{n}")).collect(),
                })
        }

        fn arb_config_and_tags() -> impl Strategy<Value = (Config, BTreeSet<String>)> {
            let collections =
                (0usize..4).prop_flat_map(|n| (0..n).map(arb_collection).collect::<Vec<_>>());
            let active = prop::collection::btree_set("[a-z]{1,4}", 0..6);
            (collections, active).prop_map(|(cols, active)| {
                let cfg = config_with(vec![mkt("m")], cols);
                (cfg, active)
            })
        }

        proptest! {
            // Every resolved plugin comes from a collection whose tags intersect
            // the active set.
            #[test]
            fn every_plugin_from_active_collection((cfg, active) in arb_config_and_tags()) {
                let resolved = resolve_plugins(&cfg, &active).expect("resolve");
                for p in &resolved.plugins {
                    let col = cfg
                        .plugin_collection
                        .iter()
                        .find(|c| c.name == p.collection)
                        .expect("provenance maps to a declared collection");
                    prop_assert!(col.when.iter().any(|t| active.contains(t)));
                }
            }

            // No duplicate (marketplace, plugin) pairs in the output.
            #[test]
            fn output_has_no_duplicate_plugins((cfg, active) in arb_config_and_tags()) {
                let resolved = resolve_plugins(&cfg, &active).expect("resolve");
                let mut seen = HashSet::new();
                for p in &resolved.plugins {
                    prop_assert!(seen.insert((p.marketplace.clone(), p.plugin.clone())));
                }
            }

            // Resolution is a pure function of (config, tags).
            #[test]
            fn resolution_is_deterministic((cfg, active) in arb_config_and_tags()) {
                let a = resolve_plugins(&cfg, &active).expect("resolve");
                let b = resolve_plugins(&cfg, &active).expect("resolve");
                prop_assert_eq!(a, b);
            }
        }
    }
}
