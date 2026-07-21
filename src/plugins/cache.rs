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
/// on first use. Returns `SyncError::Other` for path source resolution errors or
/// when git HEAD cannot be resolved after a successful clone (broken clone).
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
/// on first use. Returns `SyncError::Other` for path source resolution errors or
/// when git HEAD cannot be resolved after a successful clone (broken clone).
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
    // Marketplace name comes from user config; guard before joining into the
    // cache path (this name also flows into remove_dir_all below when the
    // source is pinned). Fixes #384, #534.
    if !crate::paths::is_valid_short_name(&m.name) {
        let name = &m.name;
        return Err(SyncError::Other(anyhow::anyhow!(
            "marketplace name '{name}' is not a valid name"
        )));
    }

    let dest = marketplace_path(cache_dir, &m.name);
    let pinned = split_source_ref(&m.source).1.is_some();

    if dest.join(".git").exists() {
        if refresh {
            if pinned {
                // #496: a pinned source is frozen by definition — pulling would
                // fast-forward past the pin. Re-clone fresh instead so a
                // refresh always converges to exactly what the pin specifies,
                // including when the user bumps the pinned ref in config (the
                // cache path is keyed by name only, not source, so the stale
                // clone must be removed explicitly).
                //
                // #536: clone into a staging dir first, alongside `dest`, so a
                // slow or failing clone never touches the working clone — only
                // a confirmed-successful clone gets swapped in via `rename`
                // (near-instant), collapsing the "dest doesn't exist" window
                // from the whole clone duration down to a couple of syscalls.
                let staging = dest.with_file_name(format!("{}.{}.tmp", m.name, std::process::id()));
                let _ = std::fs::remove_dir_all(&staging);
                git.clone(&m.source, &staging)
                    .map_err(|e| SyncError::CloneFailed {
                        name: m.name.clone(),
                        source: e,
                    })?;
                if let Err(e) = std::fs::remove_dir_all(&dest) {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(SyncError::Other(anyhow::anyhow!(
                        "removing stale pinned clone at {}: {e}",
                        dest.display()
                    )));
                }
                std::fs::rename(&staging, &dest).map_err(|e| {
                    SyncError::Other(anyhow::anyhow!(
                        "moving refreshed pinned clone into place at {}: {e}",
                        dest.display()
                    ))
                })?;
            } else {
                git.pull(&dest).map_err(SyncError::Other)?;
            }
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
    // After any git operation (clone, pull), HEAD must be resolvable. If it isn't,
    // the clone is broken and we shouldn't silently cache it with an unstable hash.
    // Clean up on error so the next invocation retries the clone instead of hitting
    // the pull path (fixes #537).
    if head.is_none() && dest.join(".git").exists() {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(SyncError::Other(anyhow::anyhow!(
            "marketplace '{}': unable to resolve git HEAD \
             (corrupted clone removed; run sync again to retry)",
            m.name
        )));
    }

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
    // #893: a single read that distinguishes NotFound (→ empty) from other I/O
    // errors (→ propagate), rather than an exists() stat that masked every stat
    // failure (e.g. EACCES) as "no manifest".
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("reading {}", manifest_path.display()))
            );
        }
    };
    let json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let plugins = json
        .get("plugins")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let name = match entry.get("name").and_then(|v| v.as_str()) {
                        Some(n) => n.to_string(),
                        None => {
                            tracing::warn!(
                                "marketplace entry skipped: missing or non-string 'name' field \
                                 (entry = {:?})",
                                entry
                            );
                            return None;
                        }
                    };
                    let raw = match entry.get("source") {
                        Some(r) => r,
                        None => {
                            tracing::warn!(
                                "marketplace entry '{}': missing 'source' field — skipping entry",
                                name
                            );
                            return None;
                        }
                    };
                    let source = if let Some(s) = raw.as_str() {
                        s.to_string()
                    } else {
                        match raw.get("url").and_then(|v| v.as_str()) {
                            Some(u) => u.to_string(),
                            None => {
                                tracing::warn!(
                                    "marketplace entry '{}': object-form source has no string \
                                     'url' field (source = {:?}) — skipping entry",
                                    name,
                                    raw
                                );
                                return None;
                            }
                        }
                    };
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
/// is false. Returns `SyncError::CloneFailed` on clone failure. Returns
/// `SyncError::Other` when git HEAD cannot be resolved after a successful clone.
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
/// is false. Returns `SyncError::CloneFailed` on clone failure. Returns
/// `SyncError::Other` when git HEAD cannot be resolved after a successful clone.
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
    // both. Fixes #384, #534.
    if !crate::paths::is_valid_short_name(marketplace) {
        return Err(SyncError::Other(anyhow::anyhow!(
            "marketplace name '{marketplace}' is not a valid name"
        )));
    }
    if !crate::paths::is_valid_short_name(plugin) {
        return Err(SyncError::Other(anyhow::anyhow!(
            "plugin name '{plugin}' in marketplace '{marketplace}' is not a valid name"
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
    // After any git operation (clone, pull), HEAD must be resolvable. If it isn't,
    // the clone is broken and we shouldn't silently cache it with an unstable hash.
    // Clean up on error so the next invocation retries the clone instead of hitting
    // the pull path (fixes #537).
    if head.is_none() && dest.join(".git").exists() {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(SyncError::Other(anyhow::anyhow!(
            "plugin '{plugin}@{marketplace}': unable to resolve git HEAD \
             (corrupted clone removed; run sync again to retry)"
        )));
    }

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

/// Split a marketplace source on its first `#`, returning `(url, Some(ref))`
/// when a ref (tag/branch/commit) is pinned, or `(source, None)` when it
/// isn't (#496). Git URLs practically never contain a literal `#` in the
/// path, so splitting on the first occurrence is unambiguous.
pub(crate) fn split_source_ref(source: &str) -> (&str, Option<&str>) {
    match source.split_once('#') {
        Some((url, r#ref)) => (url, Some(r#ref)),
        None => (source, None),
    }
}

/// Reject marketplace sources git would mishandle: leading-dash (parsed as an
/// option) and the `ext::`/`fd::` transports (run arbitrary commands on clone).
/// Also validates a pinned `#<ref>` suffix (#496) with the same rules, since
/// it flows into `git clone --branch <ref>` the same way the source flows
/// into the clone URL argument.
fn reject_unsafe_source(source: &str) -> Result<()> {
    if source.starts_with('-') {
        return Err(anyhow::anyhow!(
            "marketplace source may not start with '-': {source}"
        ));
    }
    let lower = source.to_ascii_lowercase();
    if lower.starts_with("ext::")
        || lower.starts_with("fd::")
        || lower.starts_with("file:")
        || lower.starts_with("http://")
    {
        return Err(anyhow::anyhow!(
            "marketplace source uses a disallowed git transport: {source}"
        ));
    }
    // #534: every valid git URL (https/ssh/scp-style) is pure ASCII, so
    // rejecting any non-ASCII character — not just enumerating '\0'/'\n'/'\r'
    // — closes the gap by construction: it also catches every ASCII control
    // character and every Unicode formatting character (zero-width space,
    // RTL override) that a narrower blocklist would miss.
    if let Some(ch) = source.chars().find(|c| !c.is_ascii() || c.is_control()) {
        return Err(anyhow::anyhow!(
            "marketplace source contains disallowed character {:?}: {source}",
            ch
        ));
    }
    if let Some(r#ref) = split_source_ref(source).1 {
        if r#ref.is_empty() {
            return Err(anyhow::anyhow!(
                "marketplace source has an empty pinned ref (nothing after '#'): {source}"
            ));
        }
        if r#ref.starts_with('-') {
            return Err(anyhow::anyhow!(
                "marketplace source's pinned ref may not start with '-': {source}"
            ));
        }
    }
    Ok(())
}

fn git_clone(source: &str, dest: &Path) -> Result<()> {
    let (url, pin) = split_source_ref(source);
    let mut cmd = git::secure_git();
    let cmd = git::apply_git_timeout(&mut cmd, git::DEFAULT_GIT_PLUGIN_TIMEOUT_SECS);
    cmd.args(["clone", "--depth", "1"]);
    if let Some(r#ref) = pin {
        cmd.args(["--branch", r#ref]);
    }
    let output = cmd
        .args(["--", url])
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
    let mut cmd = git::secure_git();
    let fetch_out = git::apply_git_timeout(&mut cmd, git::DEFAULT_GIT_PLUGIN_TIMEOUT_SECS)
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

/// Shared helper for `git rev-parse <ref>`. Returns the trimmed commit SHA on
/// success, `None` on any failure (IO, non-zero exit, invalid UTF-8, empty output).
fn git_rev_parse(repo: &Path, ref_name: &str) -> Option<String> {
    let output = match git::secure_git()
        .args(["rev-parse", ref_name])
        .current_dir(repo)
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(
                "git rev-parse {ref_name} failed at {}: {}",
                repo.display(),
                e
            );
            return None;
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        tracing::warn!(
            "git rev-parse {ref_name} failed at {} with exit {}: {}",
            repo.display(),
            output.status,
            stderr
        );
        return None;
    }
    match String::from_utf8(output.stdout) {
        Ok(sha) => {
            let sha = sha.trim().to_string();
            if sha.is_empty() {
                tracing::warn!(
                    "git rev-parse {ref_name} at {} returned empty output",
                    repo.display()
                );
                None
            } else {
                Some(sha)
            }
        }
        Err(e) => {
            tracing::warn!(
                "git rev-parse {ref_name} output invalid UTF-8 at {}: {}",
                repo.display(),
                e
            );
            None
        }
    }
}

/// Resolve the current HEAD sha of a git checkout, or `None` if it can't be read.
pub(crate) fn git_head(repo: &Path) -> Option<String> {
    git_rev_parse(repo, "HEAD")
}

/// Resolve a ref to its peeled commit sha using `git rev-parse <ref>^{commit}`.
/// This dereferences annotated tags to the underlying commit SHA, unlike bare
/// `git rev-parse <ref>` which returns the tag object SHA for annotated tags.
///
/// Returns `None` when the ref cannot be resolved (doesn't exist, or the
/// checked-out repo can't be read).
pub(crate) fn git_peeled_ref(repo: &Path, ref_name: &str) -> Option<String> {
    git_rev_parse(repo, &format!("{ref_name}^{{commit}}"))
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
    fn split_source_ref_parses_pinned_suffix() {
        assert_eq!(
            split_source_ref("https://github.com/example/repo.git#v1.2.3"),
            ("https://github.com/example/repo.git", Some("v1.2.3"))
        );
    }

    #[test]
    fn split_source_ref_returns_none_when_unpinned() {
        assert_eq!(
            split_source_ref("https://github.com/example/repo.git"),
            ("https://github.com/example/repo.git", None)
        );
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn prop_split_source_ref_no_panic(s in ".*") {
            let _ = split_source_ref(&s);
        }

        #[test]
        fn prop_split_source_ref_no_hash_gives_none(s in "[^#]*") {
            prop_assert_eq!(split_source_ref(&s), (s.as_str(), None));
        }

        #[test]
        fn prop_split_source_ref_url_half_never_contains_hash(
            url in "[^#]*",
            r#ref in "[^#]*",
        ) {
            let source = format!("{url}#{ref}");
            let (out_url, out_ref) = split_source_ref(&source);
            prop_assert!(!out_url.contains('#'));
            prop_assert_eq!(out_ref, Some(r#ref.as_str()));
        }

        #[test]
        fn prop_reject_unsafe_source_no_panic(s in ".*") {
            let _ = reject_unsafe_source(&s);
        }
    }

    #[test]
    fn reject_unsafe_source_rejects_leading_dash_in_pin() {
        // #496: the pinned ref is passed to `git clone --branch <ref>` — a
        // leading dash could be misread as a flag, same rationale as the
        // existing whole-source leading-dash guard.
        assert!(reject_unsafe_source("https://github.com/example/repo.git#-evil").is_err());
    }

    #[test]
    fn reject_unsafe_source_rejects_empty_pin() {
        // `url#` with nothing after the `#` would otherwise reach
        // `git clone --branch ""`, a cryptic downstream failure instead of a
        // clear validation error.
        assert!(reject_unsafe_source("https://github.com/example/repo.git#").is_err());
    }

    #[test]
    fn reject_unsafe_source_accepts_valid_pin() {
        assert!(reject_unsafe_source("https://github.com/example/repo.git#v1.2.3").is_ok());
    }

    /// Real local git repo with two commits; the first is tagged. Proves
    /// `git_clone` with a `#<tag>` pin checks out the tagged commit, not the
    /// branch tip (#496).
    #[test]
    fn git_clone_pinned_ref_checks_out_tag_not_tip() {
        let src = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(src.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        std::fs::write(src.path().join("f"), "one").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "one"]);
        run(&["tag", "-m", "v1", "v1"]);
        let tagged_sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(src.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(src.path().join("f"), "two").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "two"]);

        let dest_dir = tempfile::tempdir().unwrap();
        let dest = dest_dir.path().join("clone");
        let source = format!("{}#v1", src.path().display());
        git_clone(&source, &dest).unwrap();

        let cloned_sha = git_head(&dest).unwrap();
        assert_eq!(
            cloned_sha, tagged_sha,
            "pinned clone must check out the tag, not the branch tip"
        );

        // git_peeled_ref must also resolve the annotated tag to the same commit
        // SHA rather than returning the tag object SHA (#695).
        let peeled = git_peeled_ref(&dest, "v1").unwrap();
        assert_eq!(
            peeled, tagged_sha,
            "git_peeled_ref must dereference annotated tag to commit SHA"
        );
    }

    #[test]
    fn sync_git_recloning_pinned_source_on_refresh_instead_of_pulling() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct RecordingGit {
            clone_calls: Rc<Cell<u32>>,
            pull_calls: Rc<Cell<u32>>,
        }
        impl GitBackend for RecordingGit {
            fn clone(&self, _source: &str, dest: &Path) -> Result<()> {
                self.clone_calls.set(self.clone_calls.get() + 1);
                std::fs::create_dir_all(dest.join(".git")).unwrap();
                Ok(())
            }
            fn pull(&self, _: &Path) -> Result<()> {
                self.pull_calls.set(self.pull_calls.get() + 1);
                Ok(())
            }
            fn head(&self, _: &Path) -> Option<String> {
                Some("pinned-sha".to_string())
            }
        }

        let clone_calls = Rc::new(Cell::new(0));
        let pull_calls = Rc::new(Cell::new(0));
        let git = RecordingGit {
            clone_calls: clone_calls.clone(),
            pull_calls: pull_calls.clone(),
        };

        let m = Marketplace {
            name: "pinned".into(),
            source: "https://github.com/example/repo.git#v1.2.3".into(),
        };
        let cache = tempfile::tempdir().unwrap();

        // First sync: not yet cloned, refresh=true -> clones once.
        sync_marketplace_with(cache.path(), &m, true, &git).unwrap();
        assert_eq!(clone_calls.get(), 1);
        assert_eq!(pull_calls.get(), 0);

        // Second sync: already cloned, refresh=true -> re-clones (does not
        // pull) because the source is pinned. Guarantees convergence to
        // exactly what the pin specifies even if the pin itself changed.
        sync_marketplace_with(cache.path(), &m, true, &git).unwrap();
        assert_eq!(
            clone_calls.get(),
            2,
            "pinned source must re-clone on refresh"
        );
        assert_eq!(pull_calls.get(), 0, "pinned source must never pull");
    }

    #[test]
    fn sync_git_pinned_refresh_leaves_old_clone_intact_when_reclone_fails() {
        // #536: a failed reclone must not have already destroyed the working
        // clone — the old one stays usable until the new one is confirmed.
        struct FailingCloneGit;
        impl GitBackend for FailingCloneGit {
            fn clone(&self, _source: &str, _dest: &Path) -> Result<()> {
                anyhow::bail!("simulated network failure")
            }
            fn pull(&self, _: &Path) -> Result<()> {
                unreachable!("pinned source must never pull")
            }
            fn head(&self, _: &Path) -> Option<String> {
                Some("old-sha".to_string())
            }
        }

        let m = Marketplace {
            name: "pinned".into(),
            source: "https://github.com/example/repo.git#v1.2.3".into(),
        };
        let cache = tempfile::tempdir().unwrap();
        let dest = marketplace_path(cache.path(), &m.name);
        std::fs::create_dir_all(dest.join(".git")).unwrap();
        std::fs::write(dest.join("marker"), "old content").unwrap();

        let err = sync_marketplace_with(cache.path(), &m, true, &FailingCloneGit).unwrap_err();
        assert!(matches!(err, SyncError::CloneFailed { .. }));
        assert!(
            dest.join("marker").exists(),
            "old clone must survive a failed reclone attempt"
        );
        assert!(dest.join(".git").exists());
    }

    #[test]
    fn sync_git_pulls_unpinned_source_on_refresh_instead_of_recloning() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct RecordingGit {
            clone_calls: Rc<Cell<u32>>,
            pull_calls: Rc<Cell<u32>>,
        }
        impl GitBackend for RecordingGit {
            fn clone(&self, _source: &str, dest: &Path) -> Result<()> {
                self.clone_calls.set(self.clone_calls.get() + 1);
                std::fs::create_dir_all(dest.join(".git")).unwrap();
                Ok(())
            }
            fn pull(&self, _: &Path) -> Result<()> {
                self.pull_calls.set(self.pull_calls.get() + 1);
                Ok(())
            }
            fn head(&self, _: &Path) -> Option<String> {
                Some("head-sha".to_string())
            }
        }

        let clone_calls = Rc::new(Cell::new(0));
        let pull_calls = Rc::new(Cell::new(0));
        let git = RecordingGit {
            clone_calls: clone_calls.clone(),
            pull_calls: pull_calls.clone(),
        };

        let m = Marketplace {
            name: "floating".into(),
            source: "https://github.com/example/repo.git".into(),
        };
        let cache = tempfile::tempdir().unwrap();

        sync_marketplace_with(cache.path(), &m, true, &git).unwrap();
        assert_eq!(clone_calls.get(), 1);

        sync_marketplace_with(cache.path(), &m, true, &git).unwrap();
        assert_eq!(clone_calls.get(), 1, "unpinned source must not re-clone");
        assert_eq!(pull_calls.get(), 1, "unpinned source must pull on refresh");
    }

    #[test]
    fn git_clone_succeeds_but_head_unresolvable_returns_error() {
        // Fixes #537: if clone succeeds but we can't resolve HEAD, the clone is
        // broken and shouldn't be cached with an unstable hash. Return an error
        // to force the user to address the broken clone.
        struct SuccessfulCloneNoHead;
        impl GitBackend for SuccessfulCloneNoHead {
            fn clone(&self, _: &str, dest: &std::path::Path) -> Result<()> {
                std::fs::create_dir_all(dest.join(".git"))?;
                Ok(())
            }
            fn pull(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn head(&self, _: &std::path::Path) -> Option<String> {
                None
            }
        }

        let m = Marketplace {
            name: "corrupted".into(),
            source: "https://github.com/example/plugins".into(),
        };
        let cache = tempfile::tempdir().unwrap();
        let result = sync_marketplace_with(cache.path(), &m, true, &SuccessfulCloneNoHead);
        assert!(
            matches!(result, Err(SyncError::Other(_))),
            "expected error when clone succeeds but HEAD is unresolvable, got {result:?}"
        );
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unable to resolve git HEAD"),
            "error message should explain HEAD resolution failure, got: {msg}"
        );
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
            {"name": "external-str", "source": "https://github.com/example/external.git"},
            {"name": "external-obj", "source": {"source": "url", "url": "https://github.com/example/obj.git", "sha": "abc123"}}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert_eq!(plugins.len(), 3);
        assert_eq!(plugins[0].name, "first-party");
        assert!(!is_external_plugin_source(&plugins[0].source));
        assert_eq!(plugins[1].name, "external-str");
        assert!(is_external_plugin_source(&plugins[1].source));
        assert_eq!(plugins[2].name, "external-obj");
        assert_eq!(plugins[2].source, "https://github.com/example/obj.git");
        assert!(is_external_plugin_source(&plugins[2].source));
    }

    #[test]
    fn read_marketplace_plugins_skips_object_source_without_url() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = r#"{"plugins": [
            {"name": "good", "source": "./plugins/good"},
            {"name": "bad-obj", "source": {"source": "git", "ref": "main"}}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "good");
    }

    #[test]
    fn read_marketplace_plugins_logs_malformed_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = r#"{"plugins": [
            {"name": "good", "source": "./plugins/good"},
            {"name": 123, "source": "./bad-name-type"},
            {"source": "./missing-name"},
            {"name": "missing-source"}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        // Only the "good" entry should be included
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "good");
    }

    #[test]
    fn read_marketplace_plugins_returns_empty_when_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert!(plugins.is_empty());
    }

    // #893: a non-NotFound I/O error (EACCES) must propagate, not be swallowed
    // as an empty list the way the old exists() guard masked stat failures.
    #[cfg(unix)]
    #[test]
    fn read_marketplace_plugins_propagates_permission_error() {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        fs::create_dir(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("marketplace.json"), "{}").unwrap();
        fs::set_permissions(&plugin_dir, Permissions::from_mode(0o000)).unwrap();
        let result = read_marketplace_plugins(tmp.path());
        let readable_anyway = fs::read_dir(&plugin_dir).is_ok();
        fs::set_permissions(&plugin_dir, Permissions::from_mode(0o755)).unwrap(); // restore for cleanup
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "permission error must propagate, got {result:?}"
        );
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
                msg.contains("not a valid name"),
                "error message should reject the invalid name, got: {msg}"
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
                msg.contains("not a valid name"),
                "error message should reject the invalid name, got: {msg}"
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
                msg.contains("not a valid name"),
                "error message should reject the invalid name, got: {msg}"
            );
        }
    }

    #[test]
    fn reject_unsafe_source_rejects_dangerous_transports() {
        // Valid sources should pass
        assert!(reject_unsafe_source("https://github.com/example/repo.git").is_ok());
        assert!(reject_unsafe_source("HTTPS://github.com/example/repo.git").is_ok());
        assert!(reject_unsafe_source("git@github.com:example/repo.git").is_ok());
        assert!(reject_unsafe_source("./local/path").is_ok());

        // Dangerous sources should fail
        assert!(reject_unsafe_source("-C/evil").is_err());
        assert!(reject_unsafe_source("ext::http://example.com").is_err());
        assert!(reject_unsafe_source("fd::https://example.com").is_err());
        assert!(reject_unsafe_source("file:///home/user/.ssh").is_err());
        assert!(reject_unsafe_source("file://local/path").is_err());
        assert!(reject_unsafe_source("FILE:///path").is_err());
        assert!(reject_unsafe_source("file:/path").is_err());
        assert!(reject_unsafe_source("http://insecure.example.com").is_err());
        assert!(reject_unsafe_source("HTTP://INSECURE.COM").is_err());
    }

    #[test]
    fn reject_unsafe_source_rejects_non_ascii_and_all_control_characters() {
        // #534: the previous check only blocked '\0'/'\n'/'\r' — every valid
        // git URL is pure ASCII, so rejecting non-ASCII (which subsumes every
        // Unicode formatting character: zero-width space, RTL override, etc.)
        // and every ASCII control character (not just three of them) closes
        // the gap with no false positives.
        assert!(reject_unsafe_source("https://github.com/example/repo\t.git").is_err());
        assert!(reject_unsafe_source("https://github.com/example/repo\x7f.git").is_err());
        assert!(reject_unsafe_source("https://github.com/example/repo\u{200B}.git").is_err());
        assert!(reject_unsafe_source("https://github.com/exämple/repo.git").is_err());
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
