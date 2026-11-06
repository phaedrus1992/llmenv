//! The plugin-sync check of the selected plugins: each one is in its marketplace manifest, and
//! each external payload is synced.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use anyhow::Context as _;

use super::MarketplaceSync;
use crate::plugins::cache::MarketplacePluginEntry;

/// Check that each selected plugin exists in its synced marketplace manifest, and sync the
/// payload of each external plugin (a git URL source, not a path in the clone). A plugin of a
/// skipped marketplace is noted and not counted as missing.
pub(super) fn check_selected_plugins(
    cache_root: &Path,
    sync: &MarketplaceSync,
    selected: &[crate::plugins::resolve::ResolvedPlugin],
) -> anyhow::Result<()> {
    let mut missing_plugins: Vec<String> = Vec::new();
    let mut manifests: HashMap<&str, Vec<MarketplacePluginEntry>> = HashMap::new();
    for p in selected {
        if sync.skipped.contains(&p.marketplace) {
            eprintln!(
                "- {}@{}: skipped, its marketplace is not on this host",
                p.plugin, p.marketplace
            );
            continue;
        }
        let Some(dir) = sync.locations.get(&p.marketplace) else {
            anyhow::bail!(
                "marketplace '{}' was neither synced nor skipped",
                p.marketplace
            );
        };
        // One read for each marketplace, however many of its plugins are selected (#3447).
        let plugins = match manifests.entry(p.marketplace.as_str()) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(slot) => slot.insert(
                crate::plugins::cache::read_marketplace_plugins(dir).with_context(|| {
                    format!("reading marketplace manifest for '{}'", p.marketplace)
                })?,
            ),
        };
        let Some(entry) = plugins.iter().find(|e| e.name == p.plugin) else {
            eprintln!(
                "✗ {}@{}: not found in the manifest under {} — check that the plugin name \
                 matches an entry in that manifest",
                p.plugin,
                p.marketplace,
                dir.display()
            );
            missing_plugins.push(format!("{}@{}", p.plugin, p.marketplace));
            continue;
        };
        if !crate::plugins::cache::is_external_plugin_source(&entry.source) {
            continue;
        }
        let state =
            crate::plugins::cache::sync_plugin_entry(cache_root, &p.marketplace, entry, true)
                .with_context(|| {
                    format!("syncing external plugin '{}@{}'", p.plugin, p.marketplace)
                })?;
        let head = state.head.as_deref().unwrap_or("(unknown)");
        println!(
            "✓ {}@{} (external) → {} [{}]",
            p.plugin,
            p.marketplace,
            state.install_location.display(),
            head
        );
    }
    if !missing_plugins.is_empty() {
        anyhow::bail!(
            "{} plugin(s) not found after sync: {}",
            missing_plugins.len(),
            missing_plugins.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::{MarketplaceSync, check_selected_plugins};
    use crate::plugins::resolve::ResolvedPlugin;

    fn plugin(marketplace: &str, name: &str) -> ResolvedPlugin {
        ResolvedPlugin {
            marketplace: marketplace.to_string(),
            plugin: name.to_string(),
            collection: "c".to_string(),
            install_path: None,
            git_commit_sha: None,
        }
    }

    fn sync_of(dir: &std::path::Path, marketplace: &str, manifest: &str) -> MarketplaceSync {
        std::fs::create_dir_all(dir.join(".claude-plugin")).expect("create manifest dir");
        std::fs::write(
            dir.join(".claude-plugin").join("marketplace.json"),
            manifest,
        )
        .expect("write manifest");
        MarketplaceSync {
            locations: [(marketplace.to_string(), dir.to_path_buf())].into(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn every_plugin_of_one_manifest_is_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sync = sync_of(
            dir.path(),
            "m",
            r#"{"name": "m", "plugins": [{"name": "a", "source": "./a"},
                {"name": "b", "source": "./b"}]}"#,
        );
        check_selected_plugins(dir.path(), &sync, &[plugin("m", "a"), plugin("m", "b")])
            .expect("both plugins are in the manifest");
    }

    #[test]
    fn a_plugin_missing_from_a_shared_manifest_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sync = sync_of(
            dir.path(),
            "m",
            r#"{"name": "m", "plugins": [{"name": "a", "source": "./a"}]}"#,
        );
        let err = check_selected_plugins(dir.path(), &sync, &[plugin("m", "a"), plugin("m", "z")])
            .expect_err("z is not in the manifest");
        assert!(format!("{err:#}").contains("z@m"), "{err:#}");
    }

    #[test]
    fn an_unreadable_manifest_names_its_marketplace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sync = sync_of(dir.path(), "broken", "not json");
        let err = check_selected_plugins(dir.path(), &sync, &[plugin("broken", "a")])
            .expect_err("the manifest is not JSON");
        assert!(
            format!("{err:#}").contains("reading marketplace manifest for 'broken'"),
            "{err:#}"
        );
    }
}
