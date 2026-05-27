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
        if !collection.tags.iter().any(|t| active_tags.contains(t)) {
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
            });
        }
    }

    // Emit referenced marketplaces in config declaration order so output is
    // stable and diff-friendly regardless of plugin discovery order.
    let marketplaces = config
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

    Ok(ResolvedPlugins {
        plugins,
        marketplaces,
    })
}

#[cfg(test)]
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
            tags: tags.iter().map(|s| (*s).into()).collect(),
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
                    tags: ts,
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
                    prop_assert!(col.tags.iter().any(|t| active.contains(t)));
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
