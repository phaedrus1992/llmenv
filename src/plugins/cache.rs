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
fn marketplace_cache_root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("marketplaces")
}

/// On-disk location for a single marketplace clone.
#[must_use]
pub(crate) fn marketplace_path(cache_dir: &Path, name: &str) -> PathBuf {
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
pub(crate) fn sync_marketplace(
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

/// Replace the clone at `dest` with a fresh clone of `source`. Used for a pinned source: a pull
/// would move past the pin, and the cache path is keyed by name only, so a changed pin has to
/// replace the clone (#496, #2442).
///
/// #536: the clone goes into a staging directory beside `dest` first, so a slow or failing clone
/// never touches the working clone. Only a successful clone is swapped in with `rename`.
fn reclone_staged(
    git: &dyn GitBackend,
    source: &str,
    dest: &Path,
    name: &str,
) -> Result<(), SyncError> {
    let staging = dest.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    git.clone(source, &staging)
        .map_err(|e| SyncError::CloneFailed {
            name: name.to_string(),
            source: e,
        })?;
    if let Err(e) = std::fs::remove_dir_all(dest) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(SyncError::Other(anyhow::anyhow!(
            "removing stale pinned clone at {}: {e}",
            dest.display()
        )));
    }
    std::fs::rename(&staging, dest).map_err(|e| {
        SyncError::Other(anyhow::anyhow!(
            "moving refreshed pinned clone into place at {}: {e}",
            dest.display()
        ))
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
    let sync_start = std::time::Instant::now();
    // A hit reuses the existing clone (as-is, or fast-forwarded via `pull`);
    // a pinned refresh forces a fresh re-clone regardless of what's on disk
    // (#496), which is a deliberate cache invalidation — a miss, not a hit.
    let already_cloned = dest.join(".git").exists();
    let hit = already_cloned && !(refresh && pinned);
    let extra = format!("name={}", m.name);

    if already_cloned {
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
                reclone_staged(git, &m.source, &dest, &m.name)?;
            } else {
                git.pull(&dest).map_err(SyncError::Other)?;
            }
        }
    } else if !refresh {
        // Marketplace not yet cloned and we're not refreshing (export path).
        // This is a non-fatal condition — the marketplace just isn't available
        // on this machine yet.
        crate::cache_trace::emit_cache_trace(
            "plugin_marketplace",
            hit,
            sync_start.elapsed(),
            Some(&extra),
        );
        return Err(SyncError::NotCloned {
            name: m.name.clone(),
        });
    } else {
        // refresh=true and .git doesn't exist: attempt to clone.
        crate::paths::create_dir_owner_only(&marketplace_cache_root(cache_dir)).map_err(|e| {
            SyncError::Other(anyhow::anyhow!("creating marketplace cache root: {e}"))
        })?;
        git.clone(&m.source, &dest)
            .map_err(|e| SyncError::CloneFailed {
                name: m.name.clone(),
                source: e,
            })?;
    }
    crate::cache_trace::emit_cache_trace(
        "plugin_marketplace",
        hit,
        sync_start.elapsed(),
        Some(&extra),
    );

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
fn plugin_payload_path(cache_dir: &Path, marketplace: &str, plugin: &str) -> PathBuf {
    cache_dir
        .join("plugin-payloads")
        .join(marketplace)
        .join(plugin)
}

/// A plugin entry parsed from a marketplace's `.claude-plugin/marketplace.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplacePluginEntry {
    pub(crate) name: String,
    pub(crate) source: String,
    /// For a `git-subdir` source: the directory of the clone that holds the plugin (#2441).
    subdir: Option<String>,
}

/// Parse plugin entries from a marketplace clone's `.claude-plugin/marketplace.json`.
/// Returns an empty vec when the file is absent (bundles without a manifest are valid).
///
/// # Errors
/// Returns an error when the file exists but cannot be read or parsed.
pub(crate) fn read_marketplace_plugins(
    marketplace_dir: &Path,
) -> Result<Vec<MarketplacePluginEntry>> {
    let manifest_path = marketplace_dir
        .join(".claude-plugin")
        .join("marketplace.json");
    // #893: a single read that distinguishes NotFound (→ empty) from other I/O
    // errors (→ propagate), rather than an exists() stat that masked every stat
    // failure (e.g. EACCES) as "no manifest".
    let content = match std::fs::read_to_string(&manifest_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        r => r.with_context(|| format!("reading {}", manifest_path.display()))?,
    };
    let json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let plugins = json
        .get("plugins")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| match parse_plugin_entry(entry) {
                    Ok(parsed) => parsed,
                    // One bad entry must not hide the other plugins of the marketplace.
                    Err(e) => {
                        eprintln!("warning: {e:#} — skipping entry");
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(plugins)
}

/// Parse one `plugins[]` entry of a marketplace manifest. `Ok(None)` skips an entry that cannot
/// be used, with a warning on stderr. An `Err` is a malformed source (github, git-subdir, or a
/// pin), and the caller skips the entry with the error as the warning.
fn parse_plugin_entry(entry: &serde_json::Value) -> Result<Option<MarketplacePluginEntry>> {
    let name = match entry.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            eprintln!(
                "warning: marketplace entry skipped: missing or non-string 'name' \
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

/// The `https://github.com/<repo>.git` URL for an `owner/name` repo.
///
/// # Errors
/// `repo` is not `owner/name`.
fn github_clone_url(name: &str, repo: &str) -> Result<String> {
    // The shape first, then the meaning of each part.
    let shaped = repo.split_once('/').filter(|(_, rest)| !rest.contains('/'));
    let part_ok = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !part.starts_with('-')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !shaped.is_some_and(|(owner, repo_name)| part_ok(owner) && part_ok(repo_name)) {
        anyhow::bail!(
            "marketplace entry '{name}': github repo '{repo}' is not in owner/name form. \
             Set \"repo\" to owner/name, for example \"jeffallan/claude-skills\""
        );
    }
    Ok(format!("https://github.com/{repo}.git"))
}

/// True for a full git commit id: 40 hex digits (SHA-1) or 64 (SHA-256).
fn is_commit_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `base` with the pin of the source object appended as `#<pin>`: the `sha` when there is one,
/// else the `ref`, else no pin (#2441).
///
/// # Errors
/// `sha` is not a full commit id, or `ref` is empty, has a `#`, or is unsafe for git.
fn pin_source(name: &str, base: &str, raw: &serde_json::Value) -> Result<String> {
    // A pin that is present but not a string would otherwise read as no pin, and the plugin
    // would float on the default branch.
    for key in ["sha", "ref"] {
        if raw.get(key).is_some_and(|v| !v.is_string()) {
            anyhow::bail!(
                "marketplace entry '{name}': \"{key}\" is not a string. Set \"{key}\" to a \
                 string, or remove it"
            );
        }
    }
    let text = |key: &str| raw.get(key).and_then(|v| v.as_str());
    let pin = match (text("sha"), text("ref")) {
        (Some(sha), _) if is_commit_sha(sha) => sha,
        (Some(sha), _) => anyhow::bail!(
            "marketplace entry '{name}': sha '{sha}' is not a full commit id. \
             Set \"sha\" to 40 hex digits, or remove it"
        ),
        (None, Some(r)) if r.is_empty() || r.contains('#') => anyhow::bail!(
            "marketplace entry '{name}': ref '{r}' is empty or contains '#'. \
             Set \"ref\" to a branch, tag, or commit, or remove it to use the default branch"
        ),
        (None, Some(r)) => r,
        (None, None) => return Ok(base.to_string()),
    };
    let pinned = format!("{base}#{pin}");
    reject_unsafe_source(&pinned)
        .with_context(|| format!("marketplace entry '{name}': pin '{pin}' (ref or sha)"))?;
    Ok(pinned)
}

/// The kind of an object source that llmenv cannot fetch: `archive` and `command` (#2441).
fn unsupported_kind(raw: &serde_json::Value) -> Option<&str> {
    raw.get("source")
        .and_then(|v| v.as_str())
        .filter(|kind| matches!(*kind, "archive" | "command"))
}

/// The clone source of a `{"source": "github", "repo": "owner/name"}` object (#2440), with the
/// pin of the object appended. `Ok(None)` when the object is not a github source or has no
/// string `repo`.
///
/// # Errors
/// Errors when `repo` is not `owner/name`, or the pin is not valid (see [`pin_source`]).
fn github_repo_source(name: &str, raw: &serde_json::Value) -> Result<Option<String>> {
    if raw.get("source").and_then(|v| v.as_str()) != Some("github") {
        return Ok(None);
    }
    let Some(repo) = raw.get("repo").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    pin_source(name, &github_clone_url(name, repo)?, raw).map(Some)
}

/// The clone source and plugin directory of a `{"source": "git-subdir", "url": ..., "path": ...}`
/// object (#2441). `url` is a git URL or an `owner/name` GitHub shorthand.
///
/// # Errors
/// `url` or `path` is missing, the shorthand is malformed, `path` is not a relative path inside
/// the repo, or the pin is not valid.
fn git_subdir_source(name: &str, raw: &serde_json::Value) -> Result<(String, String)> {
    let url = raw.get("url").and_then(|v| v.as_str()).with_context(|| {
        format!("marketplace entry '{name}': git-subdir source has no string \"url\"")
    })?;
    let path = raw.get("path").and_then(|v| v.as_str()).with_context(|| {
        format!("marketplace entry '{name}': git-subdir source has no string \"path\"")
    })?;
    let is_shorthand = !url.contains(':') && !url.contains('@') && url.matches('/').count() == 1;
    let is_git_url = url.contains("://") || (url.contains('@') && url.contains(':'));
    let base = if is_shorthand {
        github_clone_url(name, url)?
    } else if is_git_url {
        url.to_string()
    } else {
        anyhow::bail!(
            "marketplace entry '{name}': git-subdir url '{url}' is neither a git URL nor \
             owner/name. Set \"url\" to a clone URL or to owner/name"
        );
    };
    Ok((pin_source(name, &base, raw)?, clean_subdir(name, path)?))
}

/// `path` as a relative directory inside a clone: no leading `./` or trailing `/`, no `..`, no
/// empty or control-character parts.
fn clean_subdir(name: &str, path: &str) -> Result<String> {
    let trimmed = path.trim_start_matches("./").trim_end_matches('/');
    let bad = trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || trimmed.chars().any(char::is_control);
    if bad {
        anyhow::bail!(
            "marketplace entry '{name}': git-subdir path '{path}' is not a relative directory \
             inside the repo. Set \"path\" to a folder such as \"tools/my-plugin\""
        );
    }
    Ok(trimmed.to_string())
}

/// True if a plugin source is an external git URL (not a relative path within
/// the marketplace clone, and not an npm package). External sources require a
/// separate clone; relative paths are served directly from the marketplace
/// directory; npm sources (#1014) resolve through the target engine's own
/// npm-install mechanism — llmenv has nothing to clone for either.
#[must_use]
pub(crate) fn is_external_plugin_source(source: &str) -> bool {
    !source.starts_with("./")
        && !source.starts_with("../")
        && !source.starts_with("npm:")
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
fn sync_external_plugin(
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
fn sync_external_plugin_with(
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
            if split_source_ref(source).1.is_some() {
                // #2442: a pinned source is frozen, so a refresh re-clones it. This also applies
                // a pin that the manifest changed since the last clone.
                reclone_staged(git, source, &dest, plugin)?;
            } else {
                git.pull(&dest).map_err(SyncError::Other)?;
            }
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
        crate::paths::create_dir_owner_only(parent)
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

/// [`sync_external_plugin`] for a parsed manifest entry. For a `git-subdir` source the install
/// location is the entry's directory inside the clone (#2441).
///
/// # Errors
/// As [`sync_external_plugin`], and `SyncError::Other` when the directory is missing from the
/// clone or leaves it through a symlink.
pub(crate) fn sync_plugin_entry(
    cache_dir: &Path,
    marketplace: &str,
    entry: &MarketplacePluginEntry,
    refresh: bool,
) -> Result<MarketplaceState, SyncError> {
    let mut state =
        sync_external_plugin(cache_dir, marketplace, &entry.name, &entry.source, refresh)?;
    if let Some(subdir) = &entry.subdir {
        state.install_location =
            subdir_root(&state.install_location, subdir, &entry.name).map_err(SyncError::Other)?;
    }
    Ok(state)
}

/// The directory `subdir` of the clone at `clone`, checked to exist and to stay inside the clone.
fn subdir_root(clone: &Path, subdir: &str, plugin: &str) -> Result<PathBuf> {
    let root = clone.join(subdir);
    let canonical = std::fs::canonicalize(&root).with_context(|| {
        format!("plugin '{plugin}': directory '{subdir}' is not in the cloned repository")
    })?;
    let clone_canonical = std::fs::canonicalize(clone)
        .with_context(|| format!("plugin '{plugin}': cannot resolve {}", clone.display()))?;
    if !canonical.starts_with(&clone_canonical) || !canonical.is_dir() {
        anyhow::bail!(
            "plugin '{plugin}': '{subdir}' is not a directory inside the cloned repository"
        );
    }
    Ok(canonical)
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
    // `<helper>::<address>` runs the `git-remote-<helper>` program, and `git://` is plaintext and
    // unauthenticated, like `http://`.
    let base = lower.split('#').next().unwrap_or(&lower);
    if base.contains("::")
        || lower.starts_with("file:")
        || lower.starts_with("http://")
        || lower.starts_with("git://")
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

/// Run one git command in `cwd`, and fail with git's scrubbed error text.
fn run_git(args: &[&str], cwd: &Path, source: &str) -> Result<()> {
    let mut cmd = git::secure_git();
    let output = git::apply_git_timeout(&mut cmd, git::DEFAULT_GIT_PLUGIN_TIMEOUT_SECS)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("spawning git {}", args.first().copied().unwrap_or("")))?;
    if !output.status.success() {
        // Git's stderr can carry embedded credentials, so scrub it (#312).
        anyhow::bail!(
            "git {} failed for {}: {}",
            args.first().copied().unwrap_or(""),
            git::sanitize_git_url(source),
            git::git_failure_detail(&output.stderr, &output.stdout, output.status)
        );
    }
    Ok(())
}

/// Clone `url` at the exact commit `sha` into `dest` (#2441). A branch clone cannot reach an
/// arbitrary commit, so this fetches the one commit and checks it out. A failure removes the
/// partial clone, so a retry starts clean.
fn git_clone_commit(url: &str, sha: &str, dest: &Path, source: &str) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let occupied = std::fs::read_dir(dest)
        .with_context(|| format!("reading {}", dest.display()))?
        .next()
        .is_some();
    if occupied {
        anyhow::bail!("{} is not empty; cannot clone into it", dest.display());
    }
    let result = run_git(&["init", "--quiet"], dest, source)
        .and_then(|()| run_git(&["remote", "add", "--", "origin", url], dest, source))
        .and_then(|()| {
            run_git(
                &["fetch", "--quiet", "--depth", "1", "origin", sha],
                dest,
                source,
            )
        })
        .and_then(|()| {
            run_git(
                &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
                dest,
                source,
            )
        });
    if result.is_err()
        && let Err(e) = std::fs::remove_dir_all(dest)
    {
        tracing::warn!("cannot remove the partial clone at {}: {e}", dest.display());
    }
    result
}

fn git_clone(source: &str, dest: &Path) -> Result<()> {
    let (url, pin) = split_source_ref(source);
    if let Some(sha) = pin.filter(|p| is_commit_sha(p)) {
        return git_clone_commit(url, sha, dest, source);
    }
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
    fn reject_unsafe_source_rejects_remote_helpers_and_plaintext_git() {
        for source in [
            "ext::sh -c evil",
            "fd::3",
            "foo::bar",
            "git://github.com/o/n.git",
            "GIT://github.com/o/n.git#v1",
            "foo::bar#v1",
        ] {
            assert!(reject_unsafe_source(source).is_err(), "{source}");
        }
        for source in [
            "https://github.com/o/n.git",
            "git@github.com:o/n.git",
            "ssh://git@host/o/n.git#v1::x",
        ] {
            // A `::` after the pin marker is part of a ref, not a helper.
            assert!(reject_unsafe_source(source).is_ok(), "{source}");
        }
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
        // Without this the fixture inherits a developer's global
        // `commit.gpgsign = true` / `tag.gpgsign = true` and every commit or
        // annotated tag below hangs/fails on a signer that can't prompt
        // (e.g. a locked/unreachable 1Password SSH-agent backend). Same
        // guard `tests/sync.rs` already uses for commit.gpgsign; this test
        // also creates an annotated tag, so it needs tag.gpgsign too.
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
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

    // #1196: the marketplace cache root must be owner-only, not just the
    // clone dest git creates inside it.
    #[cfg(unix)]
    #[test]
    fn sync_git_creates_cache_root_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        struct RecordingGit;
        impl GitBackend for RecordingGit {
            fn clone(&self, _source: &str, dest: &Path) -> Result<()> {
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

        let m = Marketplace {
            name: "fresh".into(),
            source: "https://github.com/example/repo.git".into(),
        };
        let cache = tempfile::tempdir().unwrap();

        sync_marketplace_with(cache.path(), &m, true, &RecordingGit).unwrap();

        let mode = std::fs::metadata(marketplace_cache_root(cache.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "marketplace cache root must be owner-only, got {mode:o}"
        );
    }

    /// A git backend that records its calls and marks each clone with the source it came from.
    #[derive(Default)]
    struct CallLog(std::sync::Mutex<Vec<String>>);

    impl CallLog {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl GitBackend for CallLog {
        fn clone(&self, source: &str, dest: &Path) -> Result<()> {
            self.0.lock().unwrap().push(format!("clone {source}"));
            std::fs::create_dir_all(dest.join(".git")).unwrap();
            std::fs::write(dest.join("source.txt"), source).unwrap();
            Ok(())
        }
        fn pull(&self, _: &Path) -> Result<()> {
            self.0.lock().unwrap().push("pull".into());
            Ok(())
        }
        fn head(&self, _: &Path) -> Option<String> {
            Some("abc123".into())
        }
    }

    fn sync_plugin(cache: &Path, source: &str, refresh: bool, git: &CallLog) {
        sync_external_plugin_with(cache, "market", "plugin", source, refresh, git).unwrap();
    }

    #[test]
    fn a_changed_pin_re_clones_the_plugin_payload() {
        let cache = tempfile::tempdir().unwrap();
        let git = CallLog::default();
        let v1 = "https://github.com/o/n.git#v1";
        let v2 = "https://github.com/o/n.git#v2";
        sync_plugin(cache.path(), v1, true, &git);
        sync_plugin(cache.path(), v2, true, &git);
        assert_eq!(git.calls(), [format!("clone {v1}"), format!("clone {v2}")]);
        let dest = plugin_payload_path(cache.path(), "market", "plugin");
        assert_eq!(
            std::fs::read_to_string(dest.join("source.txt")).unwrap(),
            v2
        );
        let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no staging directory is left: {leftovers:?}"
        );
    }

    #[test]
    fn an_unpinned_plugin_payload_is_pulled_and_a_no_refresh_changes_nothing() {
        let cache = tempfile::tempdir().unwrap();
        let git = CallLog::default();
        let src = "https://github.com/o/n.git";
        sync_plugin(cache.path(), src, true, &git);
        sync_plugin(cache.path(), src, true, &git);
        sync_plugin(cache.path(), "https://github.com/o/n.git#v9", false, &git);
        assert_eq!(git.calls(), [format!("clone {src}"), "pull".to_string()]);
    }

    #[test]
    fn a_failed_re_clone_keeps_the_working_payload() {
        struct FailingClone;
        impl GitBackend for FailingClone {
            fn clone(&self, _: &str, _: &Path) -> Result<()> {
                anyhow::bail!("network down")
            }
            fn pull(&self, _: &Path) -> Result<()> {
                Ok(())
            }
            fn head(&self, _: &Path) -> Option<String> {
                Some("abc".into())
            }
        }
        let cache = tempfile::tempdir().unwrap();
        sync_plugin(
            cache.path(),
            "https://github.com/o/n.git#v1",
            true,
            &CallLog::default(),
        );
        let err = sync_external_plugin_with(
            cache.path(),
            "market",
            "plugin",
            "https://github.com/o/n.git#v2",
            true,
            &FailingClone,
        );
        assert!(matches!(err, Err(SyncError::CloneFailed { .. })));
        let dest = plugin_payload_path(cache.path(), "market", "plugin");
        assert_eq!(
            std::fs::read_to_string(dest.join("source.txt")).unwrap(),
            "https://github.com/o/n.git#v1"
        );
    }

    // #1198: plugin-payloads/ is a sibling tree to marketplaces/, not nested
    // under it — #1196's marketplace_cache_root fix doesn't cover it.
    #[cfg(unix)]
    #[test]
    fn sync_external_plugin_creates_payload_parent_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        struct RecordingGit;
        impl GitBackend for RecordingGit {
            fn clone(&self, _source: &str, dest: &Path) -> Result<()> {
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
        sync_external_plugin_with(
            cache.path(),
            "market",
            "plugin",
            "https://github.com/example/plugin.git",
            true,
            &RecordingGit,
        )
        .unwrap();

        let dest = plugin_payload_path(cache.path(), "market", "plugin");
        let mode = std::fs::metadata(dest.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "plugin payload parent dir must be owner-only, got {mode:o}"
        );
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
            {"name": "external-obj", "source": {"source": "url", "url": "https://github.com/example/obj.git", "sha": "0123456789abcdef0123456789abcdef01234567"}}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert_eq!(plugins.len(), 3);
        assert_eq!(plugins[0].name, "first-party");
        assert!(!is_external_plugin_source(&plugins[0].source));
        assert_eq!(plugins[1].name, "external-str");
        assert!(is_external_plugin_source(&plugins[1].source));
        assert_eq!(plugins[2].name, "external-obj");
        assert_eq!(
            plugins[2].source,
            "https://github.com/example/obj.git#0123456789abcdef0123456789abcdef01234567"
        );
        assert!(is_external_plugin_source(&plugins[2].source));
    }

    #[test]
    fn read_marketplace_plugins_parses_npm_source() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = r#"{"plugins": [
            {"name": "claude-magic-compact", "source": {"source": "npm", "package": "claude-magic-compact", "version": "1.3.1"}},
            {"name": "no-version", "source": {"source": "npm", "package": "some-pkg"}}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "claude-magic-compact");
        assert_eq!(plugins[0].source, "npm:claude-magic-compact@1.3.1");
        assert!(
            !is_external_plugin_source(&plugins[0].source),
            "npm sources have nothing for llmenv to clone"
        );
        assert_eq!(plugins[1].source, "npm:some-pkg");
        assert!(!is_external_plugin_source(&plugins[1].source));
    }

    #[test]
    fn read_marketplace_plugins_skips_npm_source_without_package() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = r#"{"plugins": [
            {"name": "good", "source": "./plugins/good"},
            {"name": "bad-npm", "source": {"source": "npm", "version": "1.0.0"}}
        ]}"#;
        std::fs::write(plugin_dir.join("marketplace.json"), manifest).unwrap();
        let plugins = read_marketplace_plugins(tmp.path()).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "good");
    }

    fn read_manifest(plugins_json: &str) -> Result<Vec<MarketplacePluginEntry>> {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("marketplace.json"),
            format!(r#"{{"plugins": [{plugins_json}]}}"#),
        )
        .unwrap();
        read_marketplace_plugins(tmp.path())
    }

    #[test]
    fn a_github_source_with_a_repo_and_ref_becomes_a_pinned_clone_url() {
        let plugins = read_manifest(
            r#"{"name": "fullstack-dev-skills", "source":
                {"source": "github", "repo": "jeffallan/claude-skills", "ref": "plugin"}}"#,
        )
        .unwrap();
        assert_eq!(
            plugins[0].source,
            "https://github.com/jeffallan/claude-skills.git#plugin"
        );
        assert!(is_external_plugin_source(&plugins[0].source));
        assert!(reject_unsafe_source(&plugins[0].source).is_ok());
    }

    #[test]
    fn a_github_source_without_a_ref_uses_the_default_branch() {
        let plugins =
            read_manifest(r#"{"name": "p", "source": {"source": "github", "repo": "o/n"}}"#)
                .unwrap();
        assert_eq!(plugins[0].source, "https://github.com/o/n.git");
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_sha_pin_wins_over_a_ref_on_github_and_url_sources() {
        let plugins = read_manifest(&format!(
            r#"{{"name": "g", "source": {{"source": "github", "repo": "o/n", "ref": "main", "sha": "{SHA}"}}}},
               {{"name": "u", "source": {{"source": "url", "url": "https://x.example/p.git", "ref": "v2"}}}}"#
        ))
        .unwrap();
        assert_eq!(
            plugins[0].source,
            format!("https://github.com/o/n.git#{SHA}")
        );
        assert_eq!(plugins[1].source, "https://x.example/p.git#v2");
        assert!(plugins.iter().all(|p| p.subdir.is_none()));
    }

    #[test]
    fn a_sha_that_is_not_a_full_commit_id_skips_the_entry() {
        for sha in [
            "abc123",
            "",
            &SHA.to_uppercase()[..39],
            "zz23456789abcdef0123456789abcdef01234567",
        ] {
            let plugins = read_manifest(&format!(
                r#"{{"name": "bad", "source": {{"source": "url", "url": "https://x.example/p.git", "sha": "{sha}"}}}},
                   {{"name": "good", "source": "./g"}}"#
            ))
            .unwrap();
            assert_eq!(plugins.len(), 1, "{sha:?}");
        }
        let err = pin_source(
            "e",
            "https://x/y.git",
            &serde_json::json!({"sha": "abc123"}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("'e'") && err.contains("abc123") && err.contains("40 hex"),
            "{err}"
        );
    }

    #[test]
    fn a_pin_that_is_not_a_string_is_an_error_not_an_unpinned_clone() {
        for raw in [
            serde_json::json!({"sha": 123}),
            serde_json::json!({"sha": null}),
            serde_json::json!({"ref": false}),
            serde_json::json!({"ref": ["main"]}),
        ] {
            let err = pin_source("e", "https://x/y.git", &raw)
                .unwrap_err()
                .to_string();
            assert!(err.contains("'e'") && err.contains("not a string"), "{err}");
        }
    }

    proptest::proptest! {
        #[test]
        fn a_commit_id_is_exactly_40_or_64_hex_digits(text in "\\PC{0,70}") {
            let want = matches!(text.len(), 40 | 64) && text.chars().all(|c| c.is_ascii_hexdigit());
            proptest::prop_assert_eq!(is_commit_sha(&text), want);
        }

        #[test]
        fn a_valid_commit_id_is_always_accepted(sha in "[0-9a-fA-F]{40}|[0-9a-fA-F]{64}") {
            proptest::prop_assert!(is_commit_sha(&sha));
        }

        #[test]
        fn a_pinned_source_splits_back_into_its_url_and_pin(
            host in "[a-z]{1,10}",
            r in "[A-Za-z0-9][A-Za-z0-9._/-]{0,20}",
        ) {
            let base = format!("https://{host}.example/o/n.git");
            let raw = serde_json::json!({"ref": r});
            let pinned = pin_source("e", &base, &raw).unwrap();
            proptest::prop_assert_eq!(split_source_ref(&pinned), (base.as_str(), Some(r.as_str())));
        }

        #[test]
        fn a_sha_beats_a_ref_and_no_pin_leaves_the_url_alone(
            sha in "[0-9a-f]{40}",
            r in "[A-Za-z0-9]{1,10}",
        ) {
            let base = "https://x.example/o/n.git";
            let both = pin_source("e", base, &serde_json::json!({"sha": sha, "ref": r})).unwrap();
            proptest::prop_assert_eq!(both, format!("{base}#{sha}"));
            proptest::prop_assert_eq!(pin_source("e", base, &serde_json::json!({})).unwrap(), base);
        }

        #[test]
        fn a_cleaned_subdir_is_idempotent_and_has_no_unsafe_parts(path in "\\PC{0,30}") {
            if let Ok(clean) = clean_subdir("e", &path) {
                proptest::prop_assert_eq!(clean_subdir("e", &clean).unwrap(), clean.clone());
                proptest::prop_assert!(!clean.starts_with('/') && !clean.ends_with('/'));
                proptest::prop_assert!(clean.split('/').all(|p| !p.is_empty() && p != "." && p != ".."));
            }
        }
    }

    #[test]
    fn a_git_subdir_source_carries_its_directory_pin_and_shorthand() {
        let plugins = read_manifest(&format!(
            r#"{{"name": "a", "source": {{"source": "git-subdir", "url": "your-org/monorepo", "path": "tools/my-plugin"}}}},
               {{"name": "b", "source": {{"source": "git-subdir", "url": "https://git.example/r.git", "path": "./p/", "ref": "v1"}}}},
               {{"name": "c", "source": {{"source": "git-subdir", "url": "git@host:o/r.git", "path": "p", "sha": "{SHA}"}}}}"#
        ))
        .unwrap();
        assert_eq!(
            plugins[0].source,
            "https://github.com/your-org/monorepo.git"
        );
        assert_eq!(plugins[0].subdir.as_deref(), Some("tools/my-plugin"));
        assert_eq!(plugins[1].source, "https://git.example/r.git#v1");
        assert_eq!(plugins[1].subdir.as_deref(), Some("p"));
        assert_eq!(plugins[2].source, format!("git@host:o/r.git#{SHA}"));
    }

    #[test]
    fn a_git_subdir_source_with_a_bad_path_or_a_missing_field_is_skipped() {
        for source in [
            r#"{"source": "git-subdir", "url": "o/r", "path": "../up"}"#,
            r#"{"source": "git-subdir", "url": "o/r", "path": "/abs"}"#,
            r#"{"source": "git-subdir", "url": "o/r", "path": "a//b"}"#,
            r#"{"source": "git-subdir", "url": "o/r", "path": ""}"#,
            r#"{"source": "git-subdir", "url": "o/r", "path": "./"}"#,
            r#"{"source": "git-subdir", "url": "o/r"}"#,
            r#"{"source": "git-subdir", "path": "p"}"#,
            r#"{"source": "git-subdir", "url": "a/b/c", "path": "p"}"#,
        ] {
            let plugins = read_manifest(&format!(
                r#"{{"name": "bad", "source": {source}}}, {{"name": "good", "source": "./g"}}"#
            ))
            .unwrap();
            assert_eq!(plugins.len(), 1, "{source}");
        }
        let err = clean_subdir("e", "../up").unwrap_err().to_string();
        assert!(
            err.contains("'e'") && err.contains("../up") && err.contains("relative"),
            "{err}"
        );
    }

    #[test]
    fn archive_and_command_sources_are_skipped_naming_the_kind() {
        for kind in ["archive", "command"] {
            let raw = serde_json::json!({"source": kind, "url": "https://x/y.zip"});
            assert_eq!(unsupported_kind(&raw), Some(kind));
            let plugins = read_manifest(&format!(
                r#"{{"name": "bad", "source": {{"source": "{kind}", "url": "https://x/y.zip"}}}}, {{"name": "good", "source": "./g"}}"#
            ))
            .unwrap();
            assert_eq!(plugins.len(), 1, "{kind}");
        }
        assert_eq!(
            unsupported_kind(&serde_json::json!({"source": "url"})),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_subdir_root_is_inside_the_clone() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("clone");
        std::fs::create_dir_all(clone.join("tools/p")).unwrap();
        std::fs::write(clone.join("file"), "x").unwrap();
        let ok = subdir_root(&clone, "tools/p", "plug").unwrap();
        assert_eq!(ok, std::fs::canonicalize(clone.join("tools/p")).unwrap());
        let missing = subdir_root(&clone, "nope", "plug").unwrap_err().to_string();
        assert!(
            missing.contains("plug") && missing.contains("nope"),
            "{missing}"
        );
        assert!(
            subdir_root(&clone, "file", "plug").is_err(),
            "a file is not a plugin directory"
        );
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, clone.join("escape")).unwrap();
        let escaped = subdir_root(&clone, "escape", "plug")
            .unwrap_err()
            .to_string();
        assert!(
            escaped.contains("inside the cloned repository"),
            "{escaped}"
        );
    }

    #[test]
    fn a_sync_with_a_subdir_entry_installs_from_the_subdirectory() {
        struct SubdirGit;
        impl GitBackend for SubdirGit {
            fn clone(&self, _: &str, dest: &Path) -> Result<()> {
                std::fs::create_dir_all(dest.join(".git")).unwrap();
                std::fs::create_dir_all(dest.join("tools/p")).unwrap();
                Ok(())
            }
            fn pull(&self, _: &Path) -> Result<()> {
                Ok(())
            }
            fn head(&self, _: &Path) -> Option<String> {
                Some("abc".into())
            }
        }
        let cache = tempfile::tempdir().unwrap();
        let state = sync_external_plugin_with(
            cache.path(),
            "m",
            "p",
            "https://x.example/r.git",
            true,
            &SubdirGit,
        )
        .unwrap();
        let root = subdir_root(&state.install_location, "tools/p", "p").unwrap();
        assert!(root.ends_with("tools/p"));
    }

    #[test]
    fn a_commit_pin_is_fetched_and_checked_out_by_git() {
        // A local repository with two commits. The pin names the first, which is not the tip.
        let src = tempfile::tempdir().unwrap();
        let git_in = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git_in(src.path(), &["init", "--quiet"]);
        std::fs::write(src.path().join("f"), "one").unwrap();
        git_in(src.path(), &["add", "f"]);
        git_in(src.path(), &["commit", "--quiet", "-m", "one"]);
        let first = git_in(src.path(), &["rev-parse", "HEAD"]);
        std::fs::write(src.path().join("f"), "two").unwrap();
        git_in(src.path(), &["commit", "--quiet", "-am", "two"]);
        git_in(
            src.path(),
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );

        let dest = tempfile::tempdir().unwrap();
        let clone = dest.path().join("clone");
        let source = format!("{}#{first}", src.path().display());
        git_clone(&source, &clone).unwrap();
        assert_eq!(std::fs::read_to_string(clone.join("f")).unwrap(), "one");
        assert_eq!(git_head(&clone).as_deref(), Some(first.as_str()));

        let bad = dest.path().join("bad");
        let missing = format!("{}#{}", src.path().display(), "0".repeat(40));
        assert!(git_clone(&missing, &bad).is_err());
        assert!(!bad.exists(), "a failed commit clone leaves nothing behind");
    }

    #[test]
    fn a_url_wins_over_a_github_repo() {
        let plugins = read_manifest(
            r#"{"name": "p", "source": {"source": "github", "repo": "o/n", "url": "https://x.example/p.git"}}"#,
        )
        .unwrap();
        assert_eq!(plugins[0].source, "https://x.example/p.git");
    }

    #[test]
    fn a_malformed_github_repo_fails_with_the_entry_the_value_and_the_fix() {
        for repo in [
            "just-a-name",
            "a/b/c",
            "/n",
            "o/",
            "-o/n",
            "o/..",
            "o/n m",
            "o/né",
            "",
        ] {
            let raw = serde_json::json!({"source": "github", "repo": repo});
            let err = github_repo_source("bad-entry", &raw)
                .unwrap_err()
                .to_string();
            assert!(err.contains("bad-entry"), "{err}");
            assert!(err.contains(&format!("'{repo}'")), "{err}");
            assert!(err.contains("owner/name"), "{err}");
        }
    }

    #[test]
    fn a_github_ref_that_is_empty_or_has_a_hash_fails() {
        for r in ["", "a#b", "--upload-pack=x", "né", "a\u{7}b"] {
            let raw = serde_json::json!({"source": "github", "repo": "o/n", "ref": r});
            let err = github_repo_source("p", &raw).unwrap_err().to_string();
            assert!(err.contains("'p'") && err.contains("ref"), "{err}");
        }
    }

    #[test]
    fn a_malformed_github_entry_is_skipped_and_leaves_the_other_plugins() {
        let plugins = read_manifest(
            r#"{"name": "bad", "source": {"source": "github", "repo": "nope"}},
               {"name": "good", "source": "./g"}"#,
        )
        .unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "good");
    }

    #[test]
    fn a_github_source_without_a_usable_repo_is_skipped_and_names_what_is_expected() {
        for source in [
            r#"{"source": "github"}"#,
            r#"{"source": "github", "repo": 7}"#,
            r#"{"source": "git", "repo": "o/n"}"#,
        ] {
            let plugins = read_manifest(&format!(
                r#"{{"name": "bad", "source": {source}}}, {{"name": "good", "source": "./g"}}"#
            ))
            .unwrap();
            assert_eq!(plugins.len(), 1, "{source}");
            assert_eq!(plugins[0].name, "good");
        }
    }

    proptest::proptest! {
        #[test]
        fn any_owner_name_pair_of_safe_characters_becomes_a_github_url(
            owner in "[A-Za-z0-9][A-Za-z0-9._-]{0,20}",
            name in "[A-Za-z0-9][A-Za-z0-9._-]{0,20}",
        ) {
            let raw = serde_json::json!({"source": "github", "repo": format!("{owner}/{name}")});
            let got = github_repo_source("p", &raw).unwrap();
            proptest::prop_assert_eq!(got, Some(format!("https://github.com/{owner}/{name}.git")));
        }

        #[test]
        fn a_repo_without_exactly_one_slash_is_rejected(repo in "[A-Za-z0-9._-]{0,12}(/[A-Za-z0-9._-]{0,12}){2,3}|[A-Za-z0-9_-]{1,12}") {
            let raw = serde_json::json!({"source": "github", "repo": repo});
            proptest::prop_assert!(github_repo_source("p", &raw).is_err());
        }
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
