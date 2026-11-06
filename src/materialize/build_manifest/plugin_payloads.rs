//! Payload lookup for external plugins during the export path. A plugin whose payload is not
//! usable is dropped with an agent notice, never served from the wrong place.

use std::collections::HashMap;

use super::*;

/// Tells the user and the agent that a plugin is not loaded. A plugin with no install
/// path would load from the marketplace path, which does not hold its payload (#2750).
///
/// The cause is a fixed phrase, or a manifest read error, which names a path and a parse
/// position only. A payload lookup error can carry a plugin source URL, and that text goes into
/// the agent's context, so the caller logs that detail and passes a fixed phrase.
fn note_unloaded_plugin(plugin: &str, marketplace: &str, cause: &str) {
    let notice = format!(
        "llmenv: plugin '{plugin}@{marketplace}' was not loaded: {cause}. Fix: run \
         `llmenv plugin-sync` to refresh the clone."
    );
    eprintln!("warning: {notice}");
    crate::agent_notice::record(notice);
}

/// The plugin entries of a marketplace manifest, and the name it declares. A missing manifest
/// has no entries and no name.
type ParsedManifest = (
    Vec<crate::plugins::cache::MarketplacePluginEntry>,
    Option<String>,
);

/// Read and parse the manifest of `marketplace`. One read feeds the entry list and the name
/// check, so both see one file.
///
/// # Errors
/// The text of the read or parse error (#893). It names the manifest path and the cause, and
/// holds no manifest content, so it can go into an agent notice.
fn parse_manifest(cache_root: &Path, marketplace: &str) -> Result<ParsedManifest, String> {
    let mkt_path = crate::plugins::cache::marketplace_path(cache_root, marketplace);
    let parsed = crate::plugins::cache::read_manifest(&mkt_path).and_then(|manifest| {
        let Some((path, json)) = manifest else {
            return Ok((Vec::new(), None));
        };
        let entries = crate::plugins::cache::manifest_plugins(&path, &json)?;
        // Claude Code names the marketplace from the manifest, not from the config key, so a
        // mismatch means the plugin never loads (#3048).
        let declared = crate::plugins::cache::manifest_name(&path, &json)?;
        Ok((entries, Some(declared)))
    });
    parsed.map_err(|e| format!("{e:#}"))
}

/// Look up stable external plugin payload paths for resolved plugins. Non-refreshing
/// (export path): an external plugin whose payload is not usable is dropped and noted, so the
/// agent never sees an install path that points nowhere. Run `llmenv plugin-sync` to restore it.
pub(super) fn sync_plugin_payloads(
    cache_root: &Path,
    plugins: Vec<crate::plugins::resolve::ResolvedPlugin>,
) -> Vec<crate::plugins::resolve::ResolvedPlugin> {
    // One parse for each marketplace, however many of its plugins are selected (#3447).
    let mut manifests: HashMap<String, Result<ParsedManifest, String>> = HashMap::new();
    plugins
        .into_iter()
        .filter_map(|mut p| {
            let parsed = manifests
                .entry(p.marketplace.clone())
                .or_insert_with(|| parse_manifest(cache_root, &p.marketplace));
            let (entries, declared) = match parsed {
                Ok(parsed) => parsed,
                Err(cause) => {
                    tracing::error!(
                        "cannot read marketplace manifest for '{}': {cause}",
                        p.marketplace
                    );
                    note_unloaded_plugin(
                        &p.plugin,
                        &p.marketplace,
                        &format!("its marketplace manifest is unreadable: {cause}"),
                    );
                    return None;
                }
            };
            if let Some(declared) = declared.as_ref().filter(|name| **name != p.marketplace) {
                tracing::error!(
                    "marketplace '{}' declares name '{declared}' in its manifest, so Claude Code \
                     registers it as '{declared}' and plugin '{}' does not load. Fix: rename the \
                     marketplace to '{declared}' in config.yaml, and use '{declared}:<plugin>'.",
                    p.marketplace,
                    p.plugin
                );
                note_unloaded_plugin(
                    &p.plugin,
                    &p.marketplace,
                    "its marketplace name differs from the manifest name",
                );
                return None;
            }
            let Some(entry) = entries.iter().find(|e| e.name == p.plugin) else {
                tracing::warn!(
                    "plugin '{}' not found in marketplace '{}' manifest — verify plugin name, \
                     look for an earlier warning that skipped its entry, or run \
                     `llmenv plugin-sync` to refresh the clone",
                    p.plugin,
                    p.marketplace
                );
                // Kind is unknown without the entry. An external plugin served from the clone
                // would load the wrong payload, so every plugin in this arm is dropped, first-party
                // ones included (#2780).
                note_unloaded_plugin(
                    &p.plugin,
                    &p.marketplace,
                    "it is missing from its marketplace manifest",
                );
                return None;
            };
            if !crate::plugins::cache::is_external_plugin_source(&entry.source) {
                return Some(p);
            }
            match crate::plugins::cache::sync_plugin_entry(cache_root, &p.marketplace, entry, false)
            {
                Ok(state) => {
                    p.install_path = Some(state.install_location.to_string_lossy().into_owned());
                    p.git_commit_sha = state.head;
                }
                Err(crate::plugins::cache::SyncError::NotCloned { .. }) => {
                    note_unloaded_plugin(&p.plugin, &p.marketplace, "it is not fetched yet");
                    return None;
                }
                Err(e) => {
                    tracing::warn!(
                        "payload lookup for '{}@{}' failed: {e:#}",
                        p.plugin,
                        p.marketplace
                    );
                    note_unloaded_plugin(&p.plugin, &p.marketplace, "its payload lookup failed");
                    return None;
                }
            }
            Some(p)
        })
        .collect()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::sync_plugin_payloads;
    use crate::plugins::resolve::ResolvedPlugin;

    #[test]
    fn sync_plugin_payloads_drops_a_plugin_missing_from_its_manifest() {
        // A missing install path reads as first-party, so the adapter would serve the plugin
        // from the clone. The plugin's kind is unknown here, so it is dropped (#2780).
        let cache = tempfile::tempdir().unwrap();
        let mkt = crate::plugins::cache::marketplace_path(cache.path(), "mkt");
        std::fs::create_dir_all(mkt.join(".claude-plugin")).unwrap();
        std::fs::write(
            mkt.join(".claude-plugin").join("marketplace.json"),
            r#"{"plugins": [{"name": "other", "source": "./plugins/other"}]}"#,
        )
        .unwrap();
        let plugins = vec![crate::plugins::resolve::ResolvedPlugin {
            marketplace: "mkt".to_string(),
            plugin: "gone".to_string(),
            collection: "c".to_string(),
            install_path: None,
            git_commit_sha: None,
        }];
        let kept = super::sync_plugin_payloads(cache.path(), plugins);
        assert!(
            kept.is_empty(),
            "a plugin absent from its manifest must be dropped: {kept:?}"
        );
    }

    #[test]
    fn a_plugin_whose_marketplace_name_differs_from_its_manifest_is_dropped() {
        // Claude Code registers the marketplace under the manifest name, so the config name
        // loads nothing. The plugin is dropped, with the fix in the log (#3048).
        let cache = tempfile::tempdir().expect("tempdir");
        let mkt = crate::plugins::cache::marketplace_path(cache.path(), "config-name");
        std::fs::create_dir_all(mkt.join(".claude-plugin")).expect("create manifest dir");
        std::fs::write(
            mkt.join(".claude-plugin").join("marketplace.json"),
            r#"{"name": "manifest-name", "plugins": [{"name": "p", "source": "./p"}]}"#,
        )
        .expect("write manifest");
        let plugins = vec![crate::plugins::resolve::ResolvedPlugin {
            marketplace: "config-name".to_string(),
            plugin: "p".to_string(),
            collection: "c".to_string(),
            install_path: None,
            git_commit_sha: None,
        }];

        let kept = super::sync_plugin_payloads(cache.path(), plugins);

        assert!(
            kept.is_empty(),
            "a mismatched marketplace must not load: {kept:?}"
        );
    }

    fn plugin(marketplace: &str, name: &str) -> ResolvedPlugin {
        ResolvedPlugin {
            marketplace: marketplace.to_string(),
            plugin: name.to_string(),
            collection: "c".to_string(),
            install_path: None,
            git_commit_sha: None,
        }
    }

    fn write_manifest(cache: &std::path::Path, marketplace: &str, text: &str) {
        let mkt = crate::plugins::cache::marketplace_path(cache, marketplace);
        std::fs::create_dir_all(mkt.join(".claude-plugin")).expect("create manifest dir");
        std::fs::write(mkt.join(".claude-plugin").join("marketplace.json"), text)
            .expect("write manifest");
    }

    #[test]
    fn every_plugin_of_one_parsed_manifest_is_kept() {
        let cache = tempfile::tempdir().expect("tempdir");
        write_manifest(
            cache.path(),
            "m",
            r#"{"name": "m", "plugins": [{"name": "a", "source": "./a"},
                {"name": "b", "source": "./b"}]}"#,
        );
        let kept = sync_plugin_payloads(cache.path(), vec![plugin("m", "a"), plugin("m", "b")]);
        let names: Vec<&str> = kept.iter().map(|p| p.plugin.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn an_unreadable_manifest_drops_each_of_its_plugins_with_its_cause() {
        // The notice list is global to the process, so the names are unique to this test.
        let cache = tempfile::tempdir().expect("tempdir");
        write_manifest(cache.path(), "unreadable-3447", "not json");
        write_manifest(
            cache.path(),
            "readable-3447",
            r#"{"name": "readable-3447", "plugins": [{"name": "c", "source": "./c"}]}"#,
        );
        let kept = sync_plugin_payloads(
            cache.path(),
            vec![
                plugin("unreadable-3447", "first-3447"),
                plugin("readable-3447", "c"),
                plugin("unreadable-3447", "second-3447"),
            ],
        );
        let names: Vec<&str> = kept.iter().map(|p| p.plugin.as_str()).collect();
        assert_eq!(names, ["c"]);
        let notices = crate::agent_notice::current().unwrap_or_default();
        for dropped in ["first-3447", "second-3447"] {
            let line = notices
                .lines()
                .find(|line| line.contains(&format!("'{dropped}@unreadable-3447' was not loaded")))
                .unwrap_or_default();
            assert!(
                line.contains("marketplace.json") && line.contains("line 1"),
                "{dropped} needs a notice with the cause: {notices}"
            );
        }
    }

    /// The manifest state of one generated marketplace.
    #[derive(Debug, Clone, Copy)]
    enum Manifest {
        Valid,
        Missing,
        Unreadable,
    }

    fn manifest_state() -> impl proptest::strategy::Strategy<Value = Manifest> {
        proptest::prop_oneof![
            proptest::strategy::Just(Manifest::Valid),
            proptest::strategy::Just(Manifest::Missing),
            proptest::strategy::Just(Manifest::Unreadable),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        /// One shared parse for each marketplace keeps and drops the same plugins, in the same
        /// order, as a separate parse for each plugin (#3447).
        #[test]
        fn the_shared_parse_matches_a_parse_for_each_plugin(
            states in proptest::collection::vec(manifest_state(), 1..=3),
            picks in proptest::collection::vec((0usize..3, 0usize..3), 0..8),
        ) {
            let cache = tempfile::tempdir().expect("tempdir");
            for (i, state) in states.iter().enumerate() {
                let name = format!("mkt{i}");
                match state {
                    Manifest::Valid => write_manifest(
                        cache.path(),
                        &name,
                        &format!(
                            r#"{{"name": "{name}", "plugins": [{{"name": "p0", "source": "./p0"}},
                                {{"name": "p1", "source": "./p1"}}]}}"#
                        ),
                    ),
                    Manifest::Missing => {}
                    Manifest::Unreadable => write_manifest(cache.path(), &name, "{"),
                }
            }
            let plugins: Vec<ResolvedPlugin> = picks
                .iter()
                .map(|(m, p)| plugin(&format!("mkt{}", m % states.len()), &format!("p{p}")))
                .collect();
            let shared = sync_plugin_payloads(cache.path(), plugins.clone());
            let separate: Vec<ResolvedPlugin> = plugins
                .into_iter()
                .flat_map(|p| sync_plugin_payloads(cache.path(), vec![p]))
                .collect();
            let key = |list: &[ResolvedPlugin]| -> Vec<(String, String)> {
                list.iter().map(|p| (p.marketplace.clone(), p.plugin.clone())).collect()
            };
            proptest::prop_assert_eq!(key(&shared), key(&separate));
        }
    }
}
