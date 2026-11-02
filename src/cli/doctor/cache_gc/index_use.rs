//! Whether a configured codebase-memory `index_path` uses a cache folder (#2929).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::paths;

/// The folder of the bundles.
fn bundles_dir() -> anyhow::Result<PathBuf> {
    Ok(paths::config_dir()?.join("bundles"))
}

/// Whether a configured `index_path` names the codebase-memory folder.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CacheUse {
    Unused,
    Live,
    /// An `index_path` or a bundle cannot be read, so the answer is not known.
    Unknown(String),
}

/// Check every `features.codebase_memory` `index_path`: the top-level entries and the entries of
/// every configured bundle, active here or not. A bundle can be active in another folder, and its
/// server then uses the folder (#2929).
pub(super) fn cache_use(config: &Config, bundles_dir: &Path, dir: &Path) -> CacheUse {
    let raws = match configured_index_paths(config, bundles_dir) {
        Ok(raws) => raws,
        Err(cause) => return CacheUse::Unknown(cause),
    };
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(canonical) => canonical,
        // A folder that does not exist holds no index in use. A fresh install has none, so this
        // must not make the state sweep run lsof (#3195 review).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CacheUse::Unused,
        Err(e) => return CacheUse::Unknown(format!("cannot resolve {}: {e}", dir.display())),
    };
    for raw in raws {
        let expanded = match paths::expand_tilde(&raw) {
            Ok(expanded) => PathBuf::from(expanded),
            Err(e) => return CacheUse::Unknown(format!("cannot expand index_path {raw}: {e:#}")),
        };
        // A path that does not exist cannot be the folder in use. Any other failure to resolve it
        // is unknown, so the sweep is skipped and not run as if the folder were unused (#3075).
        let same = expanded == dir
            || match std::fs::canonicalize(&expanded) {
                Ok(canonical) => canonical == canonical_dir,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    return CacheUse::Unknown(format!("cannot resolve index_path {raw}: {e}"));
                }
            };
        if same {
            return CacheUse::Live;
        }
    }
    CacheUse::Unused
}

/// The raw `index_path` values of the top-level config and of each bundle's `bundle.yaml`.
fn configured_index_paths(config: &Config, bundles_dir: &Path) -> Result<Vec<String>, String> {
    let index_paths = |features: Option<&crate::config::Features>| -> Vec<String> {
        features
            .iter()
            .flat_map(|f| &f.codebase_memory)
            .filter_map(|cm| cm.index_path.clone())
            .collect()
    };
    let mut raws = index_paths(config.features.as_ref());
    for bundle in &config.bundle {
        let caps = crate::merge::read_bundle_yaml(&bundles_dir.join(&bundle.name), &bundle.name)
            .map_err(|e| format!("cannot read the index_path of a bundle: {e:#}"))?;
        raws.extend(index_paths(caps.as_ref().and_then(|c| c.features.as_ref())));
    }
    Ok(raws)
}

/// Whether a features.codebase_memory `index_path` uses `dir`. The bundle folder is needed to
/// expand the paths, and an unknown bundle folder is reported as unknown (#3075).
pub(super) fn cbm_use(config: &Config, dir: &Path) -> CacheUse {
    match bundles_dir() {
        Ok(bundles) => cache_use(config, &bundles, dir),
        Err(e) if !config.bundle.is_empty() => CacheUse::Unknown(format!(
            "cannot resolve the config folder, so the index_path of each bundle is unknown: {e:#}"
        )),
        Err(e) => {
            // No bundle is configured, so no index_path needs the folder. The error is logged.
            tracing::debug!("bundles folder unknown, but no bundle needs it: {e:#}");
            cache_use(config, Path::new(""), dir)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_codebase_memory_folder_is_unused_not_unknown() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        let missing = root.path().join("codebase-memory");
        assert!(matches!(
            cache_use(&config, root.path(), &missing),
            CacheUse::Unused
        ));
    }
}
