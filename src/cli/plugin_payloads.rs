//! Payload lookup for external plugins during the export path. A plugin whose payload is not
//! usable is dropped with an agent notice, never served from the wrong place.

use super::*;

/// Tells the user and the agent that a plugin is not loaded. A plugin with no install
/// path would load from the marketplace path, which does not hold its payload (#2750).
///
/// The cause is a fixed phrase. A lookup error can carry a plugin source URL, and that text
/// goes into the agent's context, so the error detail is logged by the caller, not here.
fn note_unloaded_plugin(plugin: &str, marketplace: &str, cause: &str) {
    let notice = format!(
        "llmenv: plugin '{plugin}@{marketplace}' was not loaded: {cause}. Fix: run \
         `llmenv plugin-sync` to refresh the clone."
    );
    eprintln!("warning: {notice}");
    crate::agent_notice::record(notice);
}

/// Look up stable external plugin payload paths for resolved plugins. Non-refreshing
/// (export path): an external plugin whose payload is not usable is dropped and noted, so the
/// agent never sees an install path that points nowhere. Run `llmenv plugin-sync` to restore it.
pub(super) fn sync_plugin_payloads(
    cache_root: &Path,
    plugins: Vec<crate::plugins::resolve::ResolvedPlugin>,
) -> Vec<crate::plugins::resolve::ResolvedPlugin> {
    plugins
        .into_iter()
        .filter_map(|mut p| {
            let mkt_path = crate::plugins::cache::marketplace_path(cache_root, &p.marketplace);
            // One read feeds the entry list and the name check, so both see one file. A missing
            // manifest has no entries and no name; a read error surfaces its cause (#893).
            let parsed = crate::plugins::cache::read_manifest(&mkt_path).and_then(|manifest| {
                let Some((path, json)) = manifest else {
                    return Ok((Vec::new(), None));
                };
                let entries = crate::plugins::cache::manifest_plugins(&path, &json)?;
                // Claude Code names the marketplace from the manifest, not from the config key,
                // so a mismatch means the plugin never loads (#3048).
                let declared = crate::plugins::cache::manifest_name(&path, &json)?;
                Ok((entries, Some(declared)))
            });
            let (entries, declared) = match parsed {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!(
                        "cannot read marketplace manifest for '{}': {e:#}",
                        p.marketplace
                    );
                    note_unloaded_plugin(
                        &p.plugin,
                        &p.marketplace,
                        "its marketplace manifest is unreadable",
                    );
                    return None;
                }
            };
            if let Some(declared) = declared.filter(|name| *name != p.marketplace) {
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
                // Kind is unknown without the entry. An external plugin served from the clone
                // would load the wrong payload, so every plugin in this arm is dropped, first-party
                // ones included (#2780).
                tracing::warn!(
                    "plugin '{}' not found in marketplace '{}' manifest — verify plugin name, \
                     look for an earlier warning that skipped its entry, or run \
                     `llmenv plugin-sync` to refresh the clone",
                    p.plugin,
                    p.marketplace
                );
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
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::sync_plugin_payloads;
    use crate::plugins::resolve::ResolvedPlugin;

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
        let plugins = vec![ResolvedPlugin {
            marketplace: "config-name".to_string(),
            plugin: "p".to_string(),
            collection: "c".to_string(),
            install_path: None,
            git_commit_sha: None,
        }];

        let kept = sync_plugin_payloads(cache.path(), plugins);

        assert!(
            kept.is_empty(),
            "a mismatched marketplace must not load: {kept:?}"
        );
    }
}
