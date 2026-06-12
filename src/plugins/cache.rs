//! Shared on-disk cache for plugin marketplaces.
//!
//! Each marketplace is fetched once into `<cache_dir>/marketplaces/<name>/` and
//! shared across every materialized scope. Git sources are cloned (and refreshed
//! by `plugin sync`); local-path sources are used in place without copying. The
//! resolved git HEAD (or a path marker) is mixed into the materialized scope
//! hash so a marketplace update re-renders the scope.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use thiserror::Error;

use crate::config::{Marketplace, MarketplaceSource};
use crate::git;
use crate::paths::expand_tilde;

/// Typed errors from marketplace sync operations.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("marketplace '{name}' not yet cloned (run `llmenv plugin-sync` to fetch)")]
    NotCloned { name: String },
    #[error("git clone failed for '{name}': {source}")]
    CloneFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Where all marketplace clones live, under the llmenv cache dir.
#[must_use]
pub fn marketplace_cache_root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("marketplaces")
}

/// On-disk location for a single marketplace clone.
#[must_use]
pub fn marketplace_path(cache_dir: &Path, name: &str) -> PathBuf {
    marketplace_cache_root(cache_dir).join(name)
}

/// The post-sync state of a marketplace: where it lives on disk and a content
/// token (git HEAD sha for git sources; the canonical path for local sources)
/// that changes when the marketplace content changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceState {
    /// Absolute path the agent should load the marketplace from.
    pub install_location: PathBuf,
    /// Content token mixed into the scope hash. `Some(sha)` for a git checkout,
    /// `None` for a local path (its location is the token instead).
    pub head: Option<String>,
}

/// The git operations `sync_marketplace` needs. Abstracted behind a trait so
/// the clone/pull/head sequencing can be tested without shelling out to a real
/// `git` binary (the implementation seam, not a network mock — see [`SystemGit`]).
pub trait GitBackend {
    /// Clone `source` into `dest` (shallow). Source validation happens in the
    /// caller, before this is invoked.
    ///
    /// # Errors
    /// Returns an error if the clone fails.
    fn clone(&self, source: &str, dest: &Path) -> Result<()>;

    /// Fast-forward an existing clone at `repo` to its upstream.
    ///
    /// # Errors
    /// Returns an error if the fetch fails. A non-fast-forwardable reset is
    /// non-fatal (current checkout is kept).
    fn pull(&self, repo: &Path) -> Result<()>;

    /// Resolve the current HEAD sha of the clone at `repo`, or `None`.
    fn head(&self, repo: &Path) -> Option<String>;
}

/// `GitBackend` backed by the real `git` binary on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemGit;

impl GitBackend for SystemGit {
    fn clone(&self, source: &str, dest: &Path) -> Result<()> {
        git_clone(source, dest)
    }
    fn pull(&self, repo: &Path) -> Result<()> {
        git_pull(repo)
    }
    fn head(&self, repo: &Path) -> Option<String> {
        git_head(repo)
    }
}

/// Fetch a marketplace into the shared cache and report its state.
///
/// Git sources are cloned on first use and fast-forward-pulled on subsequent
/// syncs (only when `refresh` is set — `export` skips the network and uses
/// whatever is already cloned). Local-path sources are resolved in place; no
/// network or copy happens.
///
/// # Errors
/// Returns `SyncError::NotCloned` if a git marketplace is not yet cloned locally
/// and refresh is false. Returns `SyncError::CloneFailed` if a git clone fails
/// on first use. Returns `SyncError::Other` for path source resolution errors.
pub fn sync_marketplace(
    cache_dir: &Path,
    m: &Marketplace,
    refresh: bool,
) -> Result<MarketplaceState, SyncError> {
    sync_marketplace_with(cache_dir, m, refresh, &SystemGit)
}

/// `sync_marketplace` with an injectable git backend, for testing the
/// clone/pull/head sequencing without a real `git` binary.
///
/// # Errors
/// Returns `SyncError::NotCloned` if a git marketplace is not yet cloned locally
/// and refresh is false. Returns `SyncError::CloneFailed` if a git clone fails
/// on first use. Returns `SyncError::Other` for path source resolution errors.
pub fn sync_marketplace_with(
    cache_dir: &Path,
    m: &Marketplace,
    refresh: bool,
    git: &dyn GitBackend,
) -> Result<MarketplaceState, SyncError> {
    match m.classify_source() {
        MarketplaceSource::Path => sync_path(m),
        MarketplaceSource::Git => sync_git(cache_dir, m, refresh, git),
    }
}

fn sync_path(m: &Marketplace) -> Result<MarketplaceState, SyncError> {
    let expanded = expand_tilde(&m.source);
    let path = PathBuf::from(&expanded);
    if !path.exists() {
        return Err(SyncError::Other(anyhow::anyhow!(
            "marketplace '{}': path source does not exist: {}",
            m.name,
            path.display()
        )));
    }
    // Canonicalize so the content token is stable regardless of how the path was
    // written (symlinks, trailing slashes, `~`). The location is mixed into the
    // scope hash, so a fall-back to the non-canonical path would make the same
    // config hash differently across runs — fail loudly instead.
    let canonical = std::fs::canonicalize(&path).map_err(|e| {
        SyncError::Other(anyhow::anyhow!(
            "marketplace '{}': canonicalizing path source {}: {e}",
            m.name,
            path.display()
        ))
    })?;
    Ok(MarketplaceState {
        install_location: canonical,
        head: None,
    })
}

fn sync_git(
    cache_dir: &Path,
    m: &Marketplace,
    refresh: bool,
    git: &dyn GitBackend,
) -> Result<MarketplaceState, SyncError> {
    // Reject dangerous sources before touching the backend (real or fake): a
    // leading-dash source trips git's arg parsing, and the `ext::`/`fd::`
    // transports run arbitrary commands on clone. Validating here keeps the
    // check independent of the backend and runnable in tests.
    reject_unsafe_source(&m.source).map_err(SyncError::Other)?;
    // Marketplace name comes from user config; guard against path traversal
    // before joining into the cache path. Fixes #384.
    if crate::paths::is_unsafe_join_target(&m.name) {
        let name = &m.name;
        return Err(SyncError::Other(anyhow::anyhow!(
            "marketplace name '{name}' contains an unsafe path component"
        )));
    }

    let dest = marketplace_path(cache_dir, &m.name);

    if dest.join(".git").exists() {
        if refresh {
            git.pull(&dest).map_err(SyncError::Other)?;
        }
    } else if !refresh {
        // Marketplace not yet cloned and we're not refreshing (export path).
        // This is a non-fatal condition — the marketplace just isn't available
        // on this machine yet.
        return Err(SyncError::NotCloned {
            name: m.name.clone(),
        });
    } else {
        // refresh=true and .git doesn't exist: attempt to clone.
        std::fs::create_dir_all(marketplace_cache_root(cache_dir)).map_err(|e| {
            SyncError::Other(anyhow::anyhow!("creating marketplace cache root: {e}"))
        })?;
        git.clone(&m.source, &dest)
            .map_err(|e| SyncError::CloneFailed {
                name: m.name.clone(),
                source: e,
            })?;
    }

    let head = git.head(&dest);
    Ok(MarketplaceState {
        install_location: dest,
        head,
    })
}

/// Stable path where an external plugin payload is cached, independent of any
/// hash-keyed config dir so it survives config changes.
#[must_use]
pub fn plugin_payload_path(cache_dir: &Path, marketplace: &str, plugin: &str) -> PathBuf {
    cache_dir
        .join("plugin-payloads")
        .join(marketplace)
        .join(plugin)
}

/// A plugin entry parsed from a marketplace's `.claude-plugin/marketplace.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplacePluginEntry {
    pub name: String,
    pub source: String,
}

/// Parse plugin entries from a marketplace clone's `.claude-plugin/marketplace.json`.
/// Returns an empty vec when the file is absent (bundles without a manifest are valid).
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub fn read_marketplace_plugins(marketplace_dir: &Path) -> Result<Vec<MarketplacePluginEntry>> {
    let manifest_path = marketplace_dir
        .join(".claude-plugin")
        .join("marketplace.json");
    if !manifest_path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let plugins = json
        .get("plugins")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let name = entry.get("name")?.as_str()?.to_string();
                    let source = entry.get("source")?.as_str()?.to_string();
                    Some(MarketplacePluginEntry { name, source })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(plugins)
}

/// True if a plugin source is an external git URL (not a relative path within
/// the marketplace clone). External sources require a separate clone; relative
/// paths are served directly from the marketplace directory.
#[must_use]
pub fn is_external_plugin_source(source: &str) -> bool {
    !source.starts_with("./")
        && !source.starts_with("../")
        && source != "."
        && source != "./"
        && source != ".."
}

/// Sync an external-sourced plugin payload to the stable llmenv cache.
///
/// # Errors
/// Returns `SyncError::NotCloned` when the payload is not present and `refresh`
/// is false. Returns `SyncError::CloneFailed` on clone failure.
pub fn sync_external_plugin(
    cache_dir: &Path,
    marketplace: &str,
    plugin: &str,
    source: &str,
    refresh: bool,
) -> Result<MarketplaceState, SyncError> {
    sync_external_plugin_with(cache_dir, marketplace, plugin, source, refresh, &SystemGit)
}

/// `sync_external_plugin` with an injectable git backend for testing.
///
/// # Errors
/// Returns `SyncError::NotCloned` when the payload is not present and `refresh`
/// is false. Returns `SyncError::CloneFailed` on clone failure.
pub fn sync_external_plugin_with(
    cache_dir: &Path,
    marketplace: &str,
    plugin: &str,
    source: &str,
    refresh: bool,
    git: &dyn GitBackend,
) -> Result<MarketplaceState, SyncError> {
    reject_unsafe_source(source).map_err(SyncError::Other)?;
    // Both marketplace and plugin names are joined into the cache path; guard
    // both against path traversal. Fixes #384.
    if crate::paths::is_unsafe_join_target(marketplace) {
        return Err(SyncError::Other(anyhow::anyhow!(
            "marketplace name '{marketplace}' contains an unsafe path component"
        )));
    }
    if crate::paths::is_unsafe_join_target(plugin) {
        return Err(SyncError::Other(anyhow::anyhow!(
            "plugin name '{plugin}' in marketplace '{marketplace}' \
             contains an unsafe path component"
        )));
    }
    let dest = plugin_payload_path(cache_dir, marketplace, plugin);
    if dest.join(".git").exists() {
        if refresh {
            git.pull(&dest).map_err(SyncError::Other)?;
        }
    } else if !refresh {
        return Err(SyncError::NotCloned {
            name: format!("{plugin}@{marketplace}"),
        });
    } else {
        // Create the parent dir so git can create `dest` itself. Creating `dest`
        // directly would block re-clone after a partial failure (git clone rejects
        // non-empty directories).
        let parent = dest.parent().ok_or_else(|| {
            SyncError::Other(anyhow::anyhow!("plugin payload path has no parent"))
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Other(anyhow::anyhow!("creating plugin payload dir: {e}")))?;
        git.clone(source, &dest)
            .map_err(|e| SyncError::CloneFailed {
                name: format!("{plugin}@{marketplace}"),
                source: e,
            })?;
    }
    let head = git.head(&dest);

    let manifest = dest.join("plugin.json");
    if !manifest.exists() {
        tracing::warn!(
            "plugin manifest missing at {}; plugin may not load correctly",
            manifest.display()
        );
    }

    Ok(MarketplaceState {
        install_location: dest,
        head,
    })
}

/// Reject marketplace sources git would mishandle: leading-dash (parsed as an
/// option) and the `ext::`/`fd::` transports (run arbitrary commands on clone).
fn reject_unsafe_source(source: &str) -> Result<()> {
    if source.starts_with('-') {
        return Err(anyhow::anyhow!(
            "marketplace source may not start with '-': {source}"
        ));
    }
    if source.starts_with("ext::") || source.starts_with("fd::") {
        return Err(anyhow::anyhow!(
            "marketplace source uses a disallowed git transport: {source}"
        ));
    }
    Ok(())
}

fn git_clone(source: &str, dest: &Path) -> Result<()> {
    let output = git::secure_git()
        .args(["clone", "--depth", "1", "--", source])
        .arg(dest)
        .output()
        .context("spawning git clone")?;
    if !output.status.success() {
        // Both the source URL and git's stderr can carry embedded credentials —
        // scrub both before they reach the user's terminal (#312).
        anyhow::bail!(
            "git clone failed for {}: {}",
            git::sanitize_git_url(source),
            git::git_failure_detail(&output.stderr, &output.stdout, output.status)
        );
    }
    Ok(())
}

/// Fast-forward an existing clone to its upstream. Only invoked on an explicit
/// refresh (`plugin sync`), never during `export`, so a fetch failure is a real
/// sync failure the caller should report — not a silent best-effort. A failed
/// `reset` (no upstream change / diverged) keeps the current checkout and is
/// non-fatal: the clone is still usable, it just didn't advance.
fn git_pull(repo: &Path) -> Result<()> {
    let fetch_out = git::secure_git()
        .args(["fetch", "--depth", "1"])
        .current_dir(repo)
        .output()
        .context("spawning git fetch")?;
    if !fetch_out.status.success() {
        anyhow::bail!(
            "git fetch failed at {}: {}",
            repo.display(),
            git::git_failure_detail(&fetch_out.stderr, &fetch_out.stdout, fetch_out.status)
        );
    }
    let reset_out = git::secure_git()
        .args(["reset", "--hard", "@{u}"])
        .current_dir(repo)
        .output()
        .context("spawning git reset")?;
    if !reset_out.status.success() {
        tracing::warn!(
            "marketplace refresh did not fast-forward at {}: {}",
            repo.display(),
            git::git_failure_detail(&reset_out.stderr, &reset_out.stdout, reset_out.status)
        );
    }
    Ok(())
}

/// Resolve the current HEAD sha of a git checkout, or `None` if it can't be read.
fn git_head(repo: &Path) -> Option<String> {
    let output = match git::secure_git()
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            tracing::debug!("git rev-parse HEAD failed at {}: {}", repo.display(), e);
            return None;
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        tracing::debug!(
            "git rev-parse HEAD failed at {} with exit {}: {}",
            repo.display(),
            output.status,
            stderr
        );
        return None;
    }
    match String::from_utf8(output.stdout) {
        Ok(sha) => {
            let sha = sha.trim().to_string();
            if sha.is_empty() { None } else { Some(sha) }
        }
        Err(e) => {
            tracing::debug!(
                "git rev-parse HEAD output invalid UTF-8 at {}: {}",
                repo.display(),
                e
            );
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn path_source_resolves_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("my-plugins");
        std::fs::create_dir(&src).unwrap();
        let m = Marketplace {
            name: "local".into(),
            source: src.to_string_lossy().into_owned(),
        };
        let cache = tempfile::tempdir().unwrap();
        let state = sync_marketplace(cache.path(), &m, false).unwrap();
        assert_eq!(state.head, None);
        assert_eq!(
            std::fs::canonicalize(&state.install_location).unwrap(),
            std::fs::canonicalize(&src).unwrap()
        );
    }

    #[test]
    fn missing_path_source_errors() {
        let m = Marketplace {
            name: "gone".into(),
            source: "/nonexistent/path/to/marketplace".into(),
        };
        let cache = tempfile::tempdir().unwrap();
        assert!(sync_marketplace(cache.path(), &m, false).is_err());
    }

    #[test]
    fn git_not_cloned_on_export_returns_notcloned() {
        struct NoGit;
        impl GitBackend for NoGit {
            fn clone(&self, _: &str, _: &std::path::Path) -> Result<()> {
                unreachable!("should not attempt clone on export (refresh=false)")
            }
            fn pull(&self, _: &std::path::Path) -> Result<()> {
                unreachable!("should not attempt pull")
            }
            fn head(&self, _: &std::path::Path) -> Option<String> {
                None
            }
        }

        let m = Marketplace {
            name: "remote".into(),
            source: "https://github.com/example/plugins".into(),
        };
        let cache = tempfile::tempdir().unwrap();
        let result = sync_marketplace_with(cache.path(), &m, false, &NoGit);
        match result {
            Err(SyncError::NotCloned { name }) => {
                assert_eq!(name, "remote");
            }
            other => panic!("expected NotCloned, got {other:?}"),
        }
    }

    #[test]
    fn git_clone_failure_returns_clonefailed() {
        struct FailClone;
        impl GitBackend for FailClone {
            fn clone(&self, _: &str, _: &std::path::Path) -> Result<()> {
                anyhow::bail!("simulated clone failure")
            }
            fn pull(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn head(&self, _: &std::path::Path) -> Option<String> {
                None
            }
        }

        let m = Marketplace {
            name: "broken".into(),
            source: "https://github.com/example/plugins".into(),
        };
        let cache = tempfile::tempdir().unwrap();
        let result = sync_marketplace_with(cache.path(), &m, true, &FailClone);
        match result {
            Err(SyncError::CloneFailed { name, .. }) => {
                assert_eq!(name, "broken");
            }
            other => panic!("expected CloneFailed, got {other:?}"),
        }
    }

    #[test]
    fn cache_paths_are_under_marketplaces_dir() {
        let root = Path::new("/cache");
        assert_eq!(
            marketplace_path(root, "superpowers"),
            PathBuf::from("/cache/marketplaces/superpowers")
        );
    }

    #[test]
    fn external_source_detection_accepts_git_urls() {
        assert!(is_external_plugin_source("https://github.com/foo/bar.git"));
        assert!(is_external_plugin_source("git@github.com:foo/bar.git"));
        assert!(is_external_plugin_source(
            "https://github.com/slackapi/slack-mcp-plugin.git"
        ));
        assert!(!is_external_plugin_source("./plugins/foo"));
        assert!(!is_external_plugin_source("./claude-plugins/nbl-dev"));
        assert!(!is_external_plugin_source("./"));
        assert!(!is_external_plugin_source("."));
        assert!(!is_external_plugin_source("../traversal"));
        assert!(!is_external_plugin_source(".."));
    }

    #[test]
    fn read_marketplace_plugins_parses_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = r#"{"plugins": [
            {"name": "first-party", "source": "./plugins/first-party"},
            {"name": "external", "source": "https://github.com/example/external.git"}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "first-party");
        assert!(!is_external_plugin_source(&plugins[0].source));
        assert_eq!(plugins[1].name, "external");
        assert!(is_external_plugin_source(&plugins[1].source));
    }

    #[test]
    fn read_marketplace_plugins_returns_empty_when_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert!(plugins.is_empty());
    }

    #[test]
    fn external_plugin_sync_clones_on_refresh() {
        use std::cell::Cell;
        use std::rc::Rc;
        let cloned = Rc::new(Cell::new(false));
        let cloned2 = cloned.clone();
        struct FakeGit(Rc<Cell<bool>>);
        impl GitBackend for FakeGit {
            fn clone(&self, _source: &str, dest: &Path) -> Result<()> {
                self.0.set(true);
                std::fs::create_dir_all(dest.join(".git")).unwrap();
                Ok(())
            }
            fn pull(&self, _: &Path) -> Result<()> {
                Ok(())
            }
            fn head(&self, _: &Path) -> Option<String> {
                Some("abc123".to_string())
            }
        }
        let cache = tempfile::tempdir().unwrap();
        let result = sync_external_plugin_with(
            cache.path(),
            "my-market",
            "my-plugin",
            "https://github.com/example/plugin.git",
            true,
            &FakeGit(cloned2),
        );
        assert!(result.is_ok());
        assert!(cloned.get(), "clone should have been called");
        let state = result.unwrap();
        assert_eq!(state.head, Some("abc123".to_string()));
        assert_eq!(
            state.install_location,
            plugin_payload_path(cache.path(), "my-market", "my-plugin"),
        );
    }

    #[test]
    fn external_plugin_sync_not_cloned_on_export() {
        struct NoGit;
        impl GitBackend for NoGit {
            fn clone(&self, _: &str, _: &Path) -> Result<()> {
                unreachable!()
            }
            fn pull(&self, _: &Path) -> Result<()> {
                unreachable!()
            }
            fn head(&self, _: &Path) -> Option<String> {
                None
            }
        }
        let cache = tempfile::tempdir().unwrap();
        let result = sync_external_plugin_with(
            cache.path(),
            "my-market",
            "my-plugin",
            "https://github.com/example/plugin.git",
            false,
            &NoGit,
        );
        assert!(matches!(result, Err(SyncError::NotCloned { .. })));
    }

    #[test]
    fn plugin_payload_path_is_under_cache() {
        let root = Path::new("/cache");
        assert_eq!(
            plugin_payload_path(root, "my-market", "my-plugin"),
            PathBuf::from("/cache/plugin-payloads/my-market/my-plugin"),
        );
    }

    #[test]
    fn git_config_flags_protect_against_hooks() {
        use crate::git::GIT_CONFIG_FLAGS;
        let flags = GIT_CONFIG_FLAGS;
        assert_eq!(
            flags,
            &[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null"
            ]
        );
    }

    /// Marketplace names come from user config and must be validated before being
    /// joined into cache paths. Unsafe names (path traversal, absolute paths) must
    /// be rejected by `sync_git` so they cannot escape the cache directory. (#384)
    #[test]
    fn sync_git_rejects_unsafe_marketplace_names() {
        struct NoGit;
        impl GitBackend for NoGit {
            fn clone(&self, _: &str, _: &std::path::Path) -> Result<()> {
                unreachable!("should not reach git backend with unsafe name")
            }
            fn pull(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn head(&self, _: &std::path::Path) -> Option<String> {
                None
            }
        }

        let cache = tempfile::tempdir().unwrap();
        for bad_name in &["../escape", "/etc/passwd", "a/../../b"] {
            let m = Marketplace {
                name: (*bad_name).to_string(),
                source: "https://github.com/example/plugins".into(),
            };
            let result = sync_marketplace_with(cache.path(), &m, true, &NoGit);
            assert!(
                result.is_err(),
                "expected error for unsafe marketplace name '{bad_name}', got Ok"
            );
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("unsafe path component"),
                "error message should mention unsafe path component, got: {msg}"
            );
        }
    }

    /// Marketplace name used as a path component in `plugin_payload_path` must
    /// also be validated in `sync_external_plugin_with`. (#384)
    #[test]
    fn sync_external_plugin_rejects_unsafe_marketplace_names() {
        struct NoGit;
        impl GitBackend for NoGit {
            fn clone(&self, _: &str, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn pull(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn head(&self, _: &std::path::Path) -> Option<String> {
                None
            }
        }

        let cache = tempfile::tempdir().unwrap();
        for bad_name in &["../escape", "/abs", "a/../b"] {
            let result = sync_external_plugin_with(
                cache.path(),
                bad_name,
                "some-plugin",
                "https://github.com/example/plugin.git",
                true,
                &NoGit,
            );
            assert!(
                result.is_err(),
                "expected error for unsafe marketplace name '{bad_name}', got Ok"
            );
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("unsafe path component"),
                "error message should mention unsafe path component, got: {msg}"
            );
        }
    }

    /// Plugin name used as a path component in `plugin_payload_path` must be
    /// validated in `sync_external_plugin_with`. (#384)
    #[test]
    fn sync_external_plugin_rejects_unsafe_plugin_names() {
        struct NoGit;
        impl GitBackend for NoGit {
            fn clone(&self, _: &str, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn pull(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn head(&self, _: &std::path::Path) -> Option<String> {
                None
            }
        }

        let cache = tempfile::tempdir().unwrap();
        for bad_name in &["../escape", "/abs", "a/../b"] {
            let result = sync_external_plugin_with(
                cache.path(),
                "valid-market",
                bad_name,
                "https://github.com/example/plugin.git",
                true,
                &NoGit,
            );
            assert!(
                result.is_err(),
                "expected error for unsafe plugin name '{bad_name}', got Ok"
            );
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("unsafe path component"),
                "error message should mention unsafe path component, got: {msg}"
            );
        }
    }

    /// git_head, git_clone, and git_pull must never block waiting for credential
    /// input on a non-interactive stdin (#299). stdin is nulled centrally by
    /// `git::secure_git()` (#307), so these call sites no longer repeat the
    /// `.stdin(null())` redirect themselves.
    ///
    /// We verify the observable effect: the commands error out immediately on a
    /// bad repo rather than hanging on stdin — git_head on a non-git path returns
    /// None (not hangs), git_clone on an invalid source errors immediately, and
    /// git_pull on a non-repo errors immediately.
    #[test]
    fn git_commands_with_null_stdin_fail_fast_not_hang() {
        let tmp = tempfile::tempdir().unwrap();

        // git_head on a non-repo returns None immediately (does not hang).
        // If stdin were inherited and git prompted, this would block.
        let head = git_head(tmp.path());
        assert!(head.is_none(), "git_head on non-repo should return None");

        // git_clone on an invalid local URL fails fast (exits non-zero, no hang).
        let dest = tmp.path().join("clone_dest");
        let err = git_clone("file:///nonexistent/repo", &dest);
        assert!(
            err.is_err(),
            "git_clone on invalid source should fail, not hang"
        );

        // git_pull on a non-repo fails fast.
        let err = git_pull(tmp.path());
        assert!(err.is_err(), "git_pull on non-repo should fail, not hang");
    }
}
