//! The cache of the `pr` widget: one file for each repo directory and branch, so the status line
//! does not run `gh` on every render.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::gh_cmd::GhPrCmd;
use super::gh_lookup::GhFailure;
use super::gh_view::gh_pr_view;
use super::{PrInfo, PrLookup};

/// How long a derived `gh pr view` result stays cached before re-querying.
/// The statusline re-execs on every prompt — without this, an unauthenticated
/// or slow `gh` would run (or fail) on every single render. 60s is short
/// enough that opening/merging/closing a PR shows up within about a prompt or
/// two, long enough that a fast typing session doesn't repeat the subprocess
/// call on every render.
pub(super) const PR_CACHE_TTL_SECS: i64 = 60;

/// Cache-then-derive backend for [`resolve_pr`]'s fallback path, fully
/// parameterized (`gh`, `cache_dir`, `now`) so tests can substitute a fake
/// `gh` and a controlled clock without touching the real network or process
/// environment.
///
/// A `gh` failure is cached too, with its cause. Without the cache a timeout would stall every
/// render, and the cause keeps the failure apart from "no open PR" (#2821).
pub(super) fn derive_pr(
    gh: GhPrCmd<'_>,
    repo_dir: &Path,
    branch: &str,
    cache_dir: Option<&Path>,
    now: i64,
) -> PrLookup {
    let cache_path = cache_dir.map(|dir| pr_cache_path(dir, repo_dir, branch));
    if let Some(path) = &cache_path
        && let Some(cached) = read_pr_cache(path, now)
    {
        return cached;
    }
    let fresh = gh_pr_view(gh, repo_dir, branch);
    if let Some(path) = &cache_path {
        write_pr_cache(path, &fresh, now);
    }
    fresh
}

/// Cache file path for a (repo, branch) key: `<cache_dir>/pr-cache-<hash>`.
/// Hashed (not a sanitized literal path) so an arbitrary repo directory or
/// branch name — including one with path separators or other characters
/// invalid in a filename — always yields a safe, collision-resistant filename.
pub(super) fn pr_cache_path(cache_dir: &Path, repo_dir: &Path, branch: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(repo_dir.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(branch.as_bytes());
    cache_dir.join(format!("pr-cache-{}", hex::encode(hasher.finalize())))
}

/// On-disk shape of a cached PR lookup: the Unix timestamp it was written and
/// the result (`None` caches "no open PR" too, so that outcome doesn't
/// retrigger a `gh` call on every render either). `failure` names why `gh` gave
/// no answer; an entry from an older version has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct PrCacheEntry {
    pub(super) ts: i64,
    pub(super) pr: Option<PrInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) failure: Option<GhFailure>,
}

/// Read a still-fresh cache entry. `None` for a missing, corrupt, unreadable, or
/// expired entry; each falls through to a fresh `gh` call in [`derive_pr`]. A read
/// error other than a missing file is logged (#2821).
pub(super) fn read_pr_cache(path: &Path, now: i64) -> Option<PrLookup> {
    let contents = match crate::paths::bounded::state_text_io(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::error!(
                "pr cache {} cannot be read, so gh runs: {e}",
                path.display()
            );
            return None;
        }
    };
    let entry: PrCacheEntry = match serde_json::from_str(&contents) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::error!("pr cache {} is corrupt, so gh runs: {e}", path.display());
            return None;
        }
    };
    let lookup = match entry.failure {
        Some(failure) => Err(failure),
        None => Ok(entry.pr),
    };
    (now.saturating_sub(entry.ts) < PR_CACHE_TTL_SECS).then_some(lookup)
}

/// Best-effort cache write: a failed write only means the next render
/// re-queries `gh`, not a broken widget.
pub(super) fn write_pr_cache(path: &Path, lookup: &PrLookup, now: i64) {
    if let Some(parent) = path.parent()
        && let Err(e) = crate::paths::create_dir_owner_only(parent)
    {
        tracing::error!("pr cache dir {} unavailable: {e}", parent.display());
        return;
    }
    let entry = PrCacheEntry {
        ts: now,
        pr: lookup.clone().unwrap_or_default(),
        failure: lookup.as_ref().err().copied(),
    };
    let json = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            tracing::error!(
                "pr cache entry for {} cannot be serialized: {e}",
                path.display()
            );
            return;
        }
    };
    if let Err(e) = crate::paths::write_owner_only(path, json.as_bytes()) {
        tracing::error!("pr cache write to {} failed: {e}", path.display());
    }
}
