//! The cache of the `pr` widget: one file for each repo directory and branch, so the status line
//! does not run `gh` on every render. Design: docs/design/statusline-pr-lookup.md

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::gh_cmd::GhPrCmd;
use super::gh_lookup::GhFailure;
use super::gh_view::gh_pr_view;
use super::pr_refresh::start_refresh;
use super::{PrInfo, PrLookup};

/// How long a derived `gh pr view` answer stays cached before re-querying.
/// The statusline re-execs on every prompt — without this, an unauthenticated
/// or slow `gh` would run (or fail) on every single render. 60s is short
/// enough that opening/merging/closing a PR shows up within about a prompt or
/// two, long enough that a fast typing session doesn't repeat the subprocess
/// call on every render.
pub(super) const PR_CACHE_TTL_SECS: i64 = 60;

/// How long a failed lookup stays cached. A timeout is transient, so the retry comes sooner than
/// for an answer. The failure is still cached, so a broken `gh` does not run on every render
/// (#3889).
pub(super) const PR_FAILURE_TTL_SECS: i64 = 10;

/// How old the last PR that a lookup returned may be, and still show in place of a fresh answer.
/// Past a day, the PR may be long closed, so the widget shows the failure instead (#3889).
pub(super) const STALE_PR_MAX_AGE_SECS: i64 = 86_400;

/// Cache-then-derive backend for [`resolve_pr`]'s fallback path, fully parameterized (`gh`,
/// `cache_dir`, `now`) so tests can substitute a fake `gh` and a controlled clock without touching
/// the real network or process environment.
///
/// The `bool` is `true` when the lookup is the last PR that a lookup returned, shown because the
/// cache entry is due or its refresh failed, so the widget can show it as stale (#3889).
///
/// A `gh` failure is cached too, with its cause. Without the cache a timeout would stall every
/// render, and the cause keeps the failure apart from "no open PR" (#2821).
///
/// With `gh.background`, a cache entry that is not fresh starts a detached refresh and the render
/// uses what the cache holds, so it never waits on `gh`. With no cache entry at all there is
/// nothing to show yet, and the answer is "no PR".
pub(super) fn derive_pr(
    gh: GhPrCmd<'_>,
    repo_dir: &Path,
    branch: &str,
    cache_dir: Option<&Path>,
    now: i64,
) -> (PrLookup, bool) {
    let Some(dir) = cache_dir else {
        return (gh_pr_view(gh, repo_dir, branch), false);
    };
    let path = pr_cache_path(dir, repo_dir, branch);
    let prior = read_pr_entry(&path);
    // A fresh failure still shows the kept PR, because a failed lookup must not replace it.
    if let Some(entry) = &prior
        && entry.is_fresh(now)
    {
        return entry.answer(now);
    }
    if gh.background {
        start_refresh(dir, repo_dir, branch, now);
        return prior.map_or((Ok(None), false), |entry| entry.shown_while_refreshing(now));
    }
    refresh_in_place(gh, &path, (repo_dir, branch), prior, now)
}

/// Run `gh` now, write the cache entry, and return what the widget shows. The detached refresh
/// calls this, and so does a render with no detaching. A failed lookup keeps the last PR that a
/// lookup returned, and a lookup with no PR clears it, so a closed PR does not stay (#3889).
pub(super) fn refresh_in_place(
    gh: GhPrCmd<'_>,
    path: &Path,
    (repo_dir, branch): (&Path, &str),
    prior: Option<PrCacheEntry>,
    now: i64,
) -> (PrLookup, bool) {
    let fresh = gh_pr_view(gh, repo_dir, branch);
    let good = match &fresh {
        Ok(Some(pr)) => Some((now, pr.clone())),
        Ok(None) => None,
        Err(_) => prior.and_then(|entry| entry.good),
    };
    let entry = PrCacheEntry {
        ts: now,
        pr: fresh.clone().unwrap_or_default(),
        failure: fresh.as_ref().err().copied(),
        good,
    };
    write_pr_entry(path, &entry);
    match (&fresh, entry.kept(now)) {
        (Err(_), Some(pr)) => (Ok(Some(pr.clone())), true),
        _ => (fresh, false),
    }
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
/// no answer; an entry from an older version has none. `good` is the last PR that a lookup
/// returned, with the time of that lookup; an entry from an older version has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct PrCacheEntry {
    pub(super) ts: i64,
    pub(super) pr: Option<PrInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) failure: Option<GhFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) good: Option<(i64, PrInfo)>,
}

impl PrCacheEntry {
    /// What the entry says: the PR, no PR, or why `gh` gave no answer.
    fn lookup(&self) -> PrLookup {
        match self.failure {
            Some(failure) => Err(failure),
            None => Ok(self.pr.clone()),
        }
    }

    /// Whether the entry is young enough to use without a refresh. A failure expires sooner.
    fn is_fresh(&self, now: i64) -> bool {
        let ttl = if self.failure.is_some() {
            PR_FAILURE_TTL_SECS
        } else {
            PR_CACHE_TTL_SECS
        };
        now.saturating_sub(self.ts) < ttl
    }

    /// The last PR that a lookup returned, when it is young enough to show.
    fn kept(&self, now: i64) -> Option<&PrInfo> {
        self.good
            .as_ref()
            .filter(|(ts, _)| now.saturating_sub(*ts) < STALE_PR_MAX_AGE_SECS)
            .map(|(_, pr)| pr)
    }

    /// What the widget shows for a fresh entry: what it says, except that a failure shows the kept
    /// PR, marked stale, when there is one.
    fn answer(&self, now: i64) -> (PrLookup, bool) {
        match (self.failure, self.kept(now)) {
            (Some(_), Some(pr)) => (Ok(Some(pr.clone())), true),
            _ => (self.lookup(), false),
        }
    }

    /// What the widget shows while a refresh of this due entry runs: the kept PR, marked stale,
    /// or else what the entry says.
    fn shown_while_refreshing(&self, now: i64) -> (PrLookup, bool) {
        match self.kept(now) {
            Some(pr) => (Ok(Some(pr.clone())), true),
            None => (self.lookup(), false),
        }
    }
}

/// Read the cache entry, whatever its age. `None` for a missing, corrupt, or unreadable entry; a
/// read error other than a missing file is logged (#2821).
pub(super) fn read_pr_entry(path: &Path) -> Option<PrCacheEntry> {
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
    match serde_json::from_str(&contents) {
        Ok(entry) => Some(entry),
        Err(e) => {
            tracing::error!("pr cache {} is corrupt, so gh runs: {e}", path.display());
            None
        }
    }
}

/// Read a still-fresh cache entry. `None` for a missing, corrupt, unreadable, or expired entry.
#[cfg(test)]
pub(super) fn read_pr_cache(path: &Path, now: i64) -> Option<PrLookup> {
    read_pr_entry(path)
        .filter(|entry| entry.is_fresh(now))
        .map(|entry| entry.lookup())
}

/// Write `lookup` as a cache entry with no kept PR.
#[cfg(test)]
pub(super) fn write_pr_cache(path: &Path, lookup: &PrLookup, now: i64) {
    write_pr_entry(
        path,
        &PrCacheEntry {
            ts: now,
            pr: lookup.clone().unwrap_or_default(),
            failure: lookup.as_ref().err().copied(),
            good: None,
        },
    );
}

/// Best-effort cache write: a failed write only means the next render
/// re-queries `gh`, not a broken widget.
fn write_pr_entry(path: &Path, entry: &PrCacheEntry) {
    if let Some(parent) = path.parent()
        && let Err(e) = crate::paths::create_dir_owner_only(parent)
    {
        tracing::error!("pr cache dir {} unavailable: {e}", parent.display());
        return;
    }
    let json = match serde_json::to_string(entry) {
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
