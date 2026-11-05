//! Payload lookup for external plugins during the export path. A plugin whose payload is not
//! usable is dropped with an agent notice, never served from the wrong place.

use std::collections::HashMap;

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

/// The plugin entries of a marketplace manifest, and the name it declares. A missing manifest
/// has no entries and no name.
type ParsedManifest = (
    Vec<crate::plugins::cache::MarketplacePluginEntry>,
    Option<String>,
);

/// Read and parse the manifest of `marketplace`. One read feeds the entry list and the name
/// check, so both see one file. A read error is logged with its cause (#893) and gives `None`.
fn parse_manifest(cache_root: &Path, marketplace: &str) -> Option<ParsedManifest> {
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
    parsed
        .inspect_err(|e| {
            tracing::warn!("cannot read marketplace manifest for '{marketplace}': {e:#}");
        })
        .ok()
}

/// Look up stable external plugin payload paths for resolved plugins. Non-refreshing
/// (export path): an external plugin whose payload is not usable is dropped and noted, so the
/// agent never sees an install path that points nowhere. Run `llmenv plugin-sync` to restore it.
pub(super) fn sync_plugin_payloads(
    cache_root: &Path,
    plugins: Vec<crate::plugins::resolve::ResolvedPlugin>,
) -> Vec<crate::plugins::resolve::ResolvedPlugin> {
    // One parse for each marketplace, however many of its plugins are selected (#3447).
    let mut manifests: HashMap<String, Option<ParsedManifest>> = HashMap::new();
    plugins
        .into_iter()
        .filter_map(|mut p| {
            let parsed = manifests
                .entry(p.marketplace.clone())
                .or_insert_with(|| parse_manifest(cache_root, &p.marketplace));
            let Some((entries, declared)) = parsed.as_ref() else {
                note_unloaded_plugin(
                    &p.plugin,
                    &p.marketplace,
                    "its marketplace manifest is unreadable",
                );
                return None;
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
    fn an_unreadable_manifest_drops_each_of_its_plugins_with_a_notice() {
        let cache = tempfile::tempdir().expect("tempdir");
        write_manifest(cache.path(), "bad", "not json");
        write_manifest(
            cache.path(),
            "good",
            r#"{"name": "good", "plugins": [{"name": "c", "source": "./c"}]}"#,
        );
        let kept = sync_plugin_payloads(
            cache.path(),
            vec![plugin("bad", "a"), plugin("good", "c"), plugin("bad", "b")],
        );
        let names: Vec<&str> = kept.iter().map(|p| p.plugin.as_str()).collect();
        assert_eq!(names, ["c"]);
        let notices = crate::agent_notice::current().unwrap_or_default();
        for dropped in ["'a@bad'", "'b@bad'"] {
            assert!(
                notices.contains(&format!("plugin {dropped} was not loaded")),
                "{dropped} needs a notice: {notices}"
            );
        }
    }
}
