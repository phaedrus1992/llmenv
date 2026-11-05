//! Reads the manifest of a marketplace checkout: its plugin entries and its declared name.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{git_subdir_source, github_repo_source, pin_source, unsupported_kind};

/// A plugin entry parsed from a marketplace's `.claude-plugin/marketplace.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplacePluginEntry {
    pub(crate) name: String,
    pub(crate) source: String,
    /// For a `git-subdir` source: the directory of the clone that holds the plugin (#2441).
    pub(crate) subdir: Option<String>,
}

/// The path and the parsed JSON of the marketplace manifest, or `None` when there is none.
///
/// One read tells NotFound (no manifest) from other I/O errors, which propagate, instead of an
/// exists() stat that masked every stat failure as "no manifest" (#893). The marketplace is a
/// cloned repository, so the read is capped and refuses a FIFO (#2945).
///
/// # Errors
/// The file exists but cannot be read or is not JSON.
pub(crate) fn read_manifest(
    marketplace_dir: &Path,
) -> Result<Option<(PathBuf, serde_json::Value)>> {
    let manifest_path = marketplace_dir
        .join(".claude-plugin")
        .join("marketplace.json");
    let Some(content) = crate::paths::bounded::asset_text_optional(&manifest_path)? else {
        return Ok(None);
    };
    let json = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    Ok(Some((manifest_path, json)))
}

/// Parse plugin entries from a marketplace clone's `.claude-plugin/marketplace.json`.
/// Returns an empty vec when the file is absent (bundles without a manifest are valid).
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub(crate) fn read_marketplace_plugins(
    marketplace_dir: &Path,
) -> Result<Vec<MarketplacePluginEntry>> {
    match read_manifest(marketplace_dir)? {
        Some((manifest_path, json)) => manifest_plugins(&manifest_path, &json),
        None => Ok(Vec::new()),
    }
}

/// The plugin entries of a manifest that [`read_manifest`] read.
///
/// # Errors
/// `plugins` is present and is not a list.
pub(crate) fn manifest_plugins(
    manifest_path: &Path,
    json: &serde_json::Value,
) -> Result<Vec<MarketplacePluginEntry>> {
    let arr = match json.get("plugins") {
        None => return Ok(Vec::new()),
        Some(value) => value.as_array().with_context(|| {
            format!(
                "{} has a \"plugins\" value that is not an array. Fix: make \"plugins\" a list of \
                 plugin entries.",
                manifest_path.display()
            )
        })?,
    };
    let plugins = arr
        .iter()
        .filter_map(|entry| match parse_plugin_entry(entry) {
            Ok(parsed) => parsed,
            // One bad entry must not hide the other plugins of the marketplace.
            Err(e) => {
                eprintln!("warning: {e:#} — skipping entry");
                None
            }
        })
        .collect::<Vec<_>>();
    Ok(plugins)
}

/// Parse one `plugins[]` entry of a marketplace manifest. `Ok(None)` skips an entry that cannot
/// be used, with a warning on stderr. An `Err` is a malformed source (github, git-subdir, or a
/// pin), and the caller skips the entry with the error as the warning.
fn parse_plugin_entry(entry: &serde_json::Value) -> Result<Option<MarketplacePluginEntry>> {
    let name = match entry.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => {
            eprintln!(
                "warning: marketplace entry skipped: missing, empty, or non-string 'name' \
                 field (entry = {:?})",
                entry
            );
            return Ok(None);
        }
    };
    let raw = match entry.get("source") {
        Some(r) => r,
        None => {
            eprintln!(
                "warning: marketplace entry '{}': missing 'source' field — \
                 skipping entry",
                name
            );
            return Ok(None);
        }
    };
    let mut subdir = None;
    let source = if let Some(s) = raw.as_str() {
        s.to_string()
    } else if raw.get("source").and_then(|v| v.as_str()) == Some("npm") {
        // #1014: an npm-source object ({"source": "npm", "package": ...,
        // "version": ...}) has no URL to clone — Claude Code's own
        // `/plugin install` resolves it directly from the npm registry.
        // Encode it as a source string `is_external_plugin_source`
        // recognizes as non-external (nothing for llmenv to clone),
        // matching the "./" -prefix sentinel this field already uses
        // for local-path sources.
        match raw.get("package").and_then(|v| v.as_str()) {
            Some(pkg) => match raw.get("version").and_then(|v| v.as_str()) {
                Some(version) => format!("npm:{pkg}@{version}"),
                None => format!("npm:{pkg}"),
            },
            None => {
                eprintln!(
                    "warning: marketplace entry '{}': npm-source object has no \
                     string 'package' field (source = {:?}) — skipping entry",
                    name, raw
                );
                return Ok(None);
            }
        }
    } else if let Some(kind) = unsupported_kind(raw) {
        eprintln!(
            "warning: marketplace entry '{name}': source kind '{kind}' is not supported — \
             skipping entry"
        );
        return Ok(None);
    } else if raw.get("source").and_then(|v| v.as_str()) == Some("git-subdir") {
        let (source, path) = git_subdir_source(&name, raw)?;
        subdir = Some(path);
        source
    } else if let Some(url) = raw.get("url").and_then(|v| v.as_str()) {
        pin_source(&name, url, raw)?
    } else if let Some(github) = github_repo_source(&name, raw)? {
        github
    } else {
        eprintln!(
            "warning: marketplace entry '{name}': object-form source has no string 'url' field \
             and no github 'repo' field (source = {raw:?}) — skipping entry"
        );
        return Ok(None);
    };
    Ok(Some(MarketplacePluginEntry {
        name,
        source,
        subdir,
    }))
}

/// The name that a marketplace checkout declares in its manifest. Claude Code names a directory
/// marketplace from this field, not from the config key, so a mismatch hides its plugins (#3048).
/// `None` only when the manifest is absent. A manifest without a string `name` is an error, so the
/// check cannot pass silently (#3048).
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub(crate) fn read_marketplace_name(marketplace_dir: &Path) -> Result<Option<String>> {
    read_manifest(marketplace_dir)?
        .map(|(manifest_path, json)| manifest_name(&manifest_path, &json))
        .transpose()
}

/// The declared name of a manifest that [`read_manifest`] read.
///
/// # Errors
/// The manifest has no string `name`.
pub(crate) fn manifest_name(manifest_path: &Path, json: &serde_json::Value) -> Result<String> {
    let name = json
        .get("name")
        .and_then(serde_json::Value::as_str)
        .with_context(|| {
            format!(
                "{} has no string \"name\". Fix: add the marketplace name to the manifest.",
                manifest_path.display()
            )
        })?;
    Ok(name.to_owned())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::read_marketplace_name;

    #[test]
    fn read_marketplace_name_returns_the_declared_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".claude-plugin")).expect("create manifest dir");
        std::fs::write(
            dir.path().join(".claude-plugin").join("marketplace.json"),
            r#"{"name": "declared"}"#,
        )
        .expect("write manifest");

        let name = read_marketplace_name(dir.path()).expect("read");

        assert_eq!(name.as_deref(), Some("declared"));
    }

    #[test]
    fn read_marketplace_name_is_none_without_a_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_marketplace_name(dir.path()).expect("read"), None);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod error_tests {
    use super::{read_marketplace_name, read_marketplace_plugins};
    use proptest::prelude::*;

    fn write_manifest(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".claude-plugin")).expect("create dir");
        std::fs::write(
            dir.path().join(".claude-plugin").join("marketplace.json"),
            text,
        )
        .expect("write manifest");
        dir
    }

    #[test]
    fn a_non_string_name_is_an_error_not_an_absent_name() {
        let dir = write_manifest(r#"{"name": 5}"#);
        let err = read_marketplace_name(dir.path()).expect_err("a number is not a name");
        assert!(format!("{err:#}").contains("no string \"name\""), "{err:#}");
    }

    #[test]
    fn a_missing_name_is_an_error() {
        let dir = write_manifest(r#"{"plugins": []}"#);
        assert!(read_marketplace_name(dir.path()).is_err());
    }

    #[test]
    fn a_plugins_value_that_is_not_an_array_is_an_error() {
        let dir = write_manifest(r#"{"plugins": {"name": "p"}}"#);
        let err = read_marketplace_plugins(dir.path()).expect_err("an object is not a list");
        assert!(format!("{err:#}").contains("not an array"), "{err:#}");
    }

    proptest! {
        /// Any string name, with quotes, backslashes, newlines, or non-ASCII text, reads back
        /// unchanged.
        #[test]
        fn any_string_name_reads_back_unchanged(name in "\\PC{0,24}|[\"\\\\\n\t]{0,6}") {
            let dir = write_manifest(&serde_json::json!({ "name": name }).to_string());
            prop_assert_eq!(read_marketplace_name(dir.path()).expect("read"), Some(name));
        }

        /// A name that is not a string is an error, never a name or an absent manifest.
        #[test]
        fn a_name_of_another_type_is_an_error(
            value in prop_oneof![
                Just(serde_json::Value::Null),
                any::<bool>().prop_map(serde_json::Value::from),
                any::<i64>().prop_map(serde_json::Value::from),
                Just(serde_json::json!([])),
                Just(serde_json::json!({})),
            ],
        ) {
            let dir = write_manifest(&serde_json::json!({ "name": value }).to_string());
            prop_assert!(read_marketplace_name(dir.path()).is_err());
        }

        /// Untrusted manifest entries never panic, and an accepted entry always has a name.
        #[test]
        fn parse_plugin_entry_never_panics_and_accepts_only_named_entries(
            name in proptest::option::of(".{0,20}"),
            source in proptest::option::of(".{0,20}"),
        ) {
            let mut obj = serde_json::Map::new();
            if let Some(n) = name { obj.insert("name".into(), n.into()); }
            if let Some(s) = source { obj.insert("source".into(), s.into()); }
            let entry = serde_json::Value::Object(obj);
            if let Ok(Some(parsed)) = super::parse_plugin_entry(&entry) {
                prop_assert!(!parsed.name.is_empty());
            }
        }
    }
}
