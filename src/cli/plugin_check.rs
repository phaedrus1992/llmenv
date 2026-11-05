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
