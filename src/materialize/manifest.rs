//! The `.llmenv-manifest.json` ownership manifest written into every
//! materialized folder (#196).
//!
//! One dotfile serves two jobs, identically in both [`HashingMode`]s:
//!
//! - **Drift detection** (`check-stale`, `doctor`): the recorded
//!   [`CacheManifest::content_hash`] is compared against the hash llmenv would
//!   render now. A difference means the config changed in place and the running
//!   agent should relaunch. This is one code path — no strict-vs-version branch.
//! - **Reconciliation** (version-mode re-render): the recorded
//!   [`CacheManifest::owned`] set is exactly what llmenv wrote last time.
//!   Deleting `previous − current` removes ghost files (a dropped `rules/*.md`,
//!   a removed plugin) without ever touching files llmenv doesn't own (Claude's
//!   runtime state, third-party plugin state — see #175).
//!
//! [`HashingMode`]: crate::config::HashingMode

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The dotfile name written into every materialized folder.
pub const MANIFEST_FILE: &str = ".llmenv-manifest.json";

/// Records what llmenv owns in a materialized folder: the content hash (for
/// drift detection) and the set of llmenv-written paths (for reconciliation).
/// Paths are stored relative to the materialized folder, forward-slash
/// normalized so the manifest round-trips across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheManifest {
    /// The content hash llmenv rendered (see [`super::cache::hash_manifest`]).
    pub content_hash: String,
    /// llmenv-owned paths in this folder, relative + `/`-separated, sorted.
    pub owned: BTreeSet<String>,
}

impl CacheManifest {
    /// Build a manifest from a content hash and the set of owned relative
    /// paths. Paths are normalized to forward slashes and the dotfile itself is
    /// never recorded as owned (it is metadata, not content).
    ///
    /// A path that would escape the materialized folder when joined (`..`
    /// components or an absolute path) is dropped rather than recorded: the
    /// owned set drives `remove_file` during reconciliation, so a traversal
    /// path that survived into the manifest would let a re-render delete files
    /// outside the cache. llmenv only ever writes safe relative paths, so a
    /// rejected entry means a corrupt or tampered manifest, not lost ownership.
    #[must_use]
    pub fn new(content_hash: impl Into<String>, owned: impl IntoIterator<Item = PathBuf>) -> Self {
        let owned = owned
            .into_iter()
            .map(|p| normalize_rel(&p))
            .filter(|p| p != MANIFEST_FILE && !p.is_empty())
            .filter(|p| !crate::paths::is_unsafe_join_target(p))
            .collect();
        Self {
            content_hash: content_hash.into(),
            owned,
        }
    }

    /// Read the manifest from `folder/.llmenv-manifest.json`. Returns `Ok(None)`
    /// when the dotfile is absent (a folder llmenv never wrote, or a pre-#196
    /// folder) or unparseable (treat a corrupt manifest as "no prior knowledge"
    /// rather than failing the whole render — the worst case is a stale ghost
    /// file, never data loss, since reconciliation only deletes recorded paths).
    ///
    /// # Errors
    /// Returns an error only on an I/O failure that is *not* "file not found"
    /// (e.g. a permissions error reading an existing dotfile).
    pub fn read(folder: &Path) -> anyhow::Result<Option<Self>> {
        let path = folder.join(MANIFEST_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "reading cache manifest {}: {e}",
                path.display()
            )),
        }
    }

    /// Write the manifest to `folder/.llmenv-manifest.json` with owner-only
    /// permissions. Written *last* in a re-render, so an interrupted render
    /// leaves the previous manifest pointing at a still-consistent owned set.
    ///
    /// # Errors
    /// Returns an error if serialization or the atomic write fails.
    pub fn write(&self, folder: &Path) -> anyhow::Result<()> {
        let path = folder.join(MANIFEST_FILE);
        let json = serde_json::to_string_pretty(self)?;
        crate::paths::write_owner_only_atomic(&path, json.as_bytes())
            .map_err(|e| anyhow::anyhow!("writing cache manifest {}: {e}", path.display()))?;
        Ok(())
    }

    /// Paths owned by `self` (the previous render) but not by `current` — the
    /// ghost files a version-mode re-render must delete. Everything outside this
    /// set is left untouched: either still-owned (rewritten in place) or never
    /// llmenv's to begin with.
    #[must_use]
    pub fn stale_against(&self, current: &CacheManifest) -> Vec<String> {
        self.owned.difference(&current.owned).cloned().collect()
    }
}

/// Normalize a relative path to a forward-slash string for stable, portable
/// storage. Backslashes (Windows separators) are folded to `/` so a manifest
/// written on one platform reconciles correctly on another.
fn normalize_rel(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn new_drops_the_dotfile_and_empty_paths() {
        let m = CacheManifest::new(
            "abc",
            vec![
                PathBuf::from("CLAUDE.md"),
                PathBuf::from(MANIFEST_FILE),
                PathBuf::new(),
            ],
        );
        assert_eq!(m.content_hash, "abc");
        assert_eq!(
            m.owned,
            BTreeSet::from(["CLAUDE.md".to_string()]),
            "the manifest never records itself or empty paths"
        );
    }

    #[test]
    fn new_drops_traversal_and_absolute_paths() {
        // The owned set drives remove_file during reconciliation; a path that
        // escapes the folder must never be recorded (#196 path-traversal).
        let m = CacheManifest::new(
            "abc",
            vec![
                PathBuf::from("CLAUDE.md"),
                PathBuf::from("../../../etc/passwd"),
                PathBuf::from("/etc/shadow"),
                PathBuf::from("rules/../../escape.md"),
            ],
        );
        assert_eq!(
            m.owned,
            BTreeSet::from(["CLAUDE.md".to_string()]),
            "only the safe relative path is recorded"
        );
    }

    #[test]
    fn read_absent_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(CacheManifest::read(tmp.path()).unwrap(), None);
    }

    #[test]
    fn read_corrupt_is_none_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MANIFEST_FILE), b"{ not json").unwrap();
        // A corrupt manifest must degrade to "no prior knowledge", not abort
        // the render — reconciliation only ever deletes *recorded* paths, so
        // the worst case is a lingering ghost file, never data loss.
        assert_eq!(CacheManifest::read(tmp.path()).unwrap(), None);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let m = CacheManifest::new(
            "deadbeef",
            vec![PathBuf::from("settings.json"), PathBuf::from("rules/a.md")],
        );
        m.write(tmp.path()).unwrap();
        let back = CacheManifest::read(tmp.path()).unwrap().unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn stale_against_is_previous_minus_current() {
        let prev = CacheManifest::new(
            "h1",
            vec![
                PathBuf::from("CLAUDE.md"),
                PathBuf::from("rules/old.md"),
                PathBuf::from("settings.json"),
            ],
        );
        let cur = CacheManifest::new(
            "h2",
            vec![PathBuf::from("CLAUDE.md"), PathBuf::from("settings.json")],
        );
        let stale = prev.stale_against(&cur);
        assert_eq!(stale, vec!["rules/old.md".to_string()]);
    }

    #[test]
    fn stale_against_empty_when_current_superset() {
        let prev = CacheManifest::new("h1", vec![PathBuf::from("CLAUDE.md")]);
        let cur = CacheManifest::new(
            "h2",
            vec![PathBuf::from("CLAUDE.md"), PathBuf::from("new.md")],
        );
        assert!(prev.stale_against(&cur).is_empty());
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        // Safe relative path segments — no traversal, no separators that
        // `new()` would reject, so what goes in is what comes back out.
        fn arb_rel() -> impl Strategy<Value = String> {
            "[a-z0-9_]{1,8}(/[a-z0-9_]{1,8}){0,2}"
        }

        fn arb_manifest() -> impl Strategy<Value = CacheManifest> {
            (
                "[a-f0-9]{0,64}",
                prop::collection::vec(arb_rel().prop_map(PathBuf::from), 0..8),
            )
                .prop_map(|(hash, owned)| CacheManifest::new(hash, owned))
        }

        proptest! {
            #[test]
            fn serde_roundtrips(m in arb_manifest()) {
                let json = serde_json::to_string(&m).unwrap();
                let back: CacheManifest = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back, m, "manifest must survive a JSON round-trip");
            }

            #[test]
            fn stale_is_previous_minus_current(prev in arb_manifest(), cur in arb_manifest()) {
                let stale: BTreeSet<String> = prev.stale_against(&cur).into_iter().collect();
                // Every stale path was owned before and is not owned now.
                prop_assert!(stale.is_subset(&prev.owned), "stale ⊆ previous.owned");
                prop_assert!(
                    stale.is_disjoint(&cur.owned),
                    "stale never names a still-owned (current) path"
                );
                // Completeness: nothing in previous-but-not-current is missed.
                let expected: BTreeSet<String> =
                    prev.owned.difference(&cur.owned).cloned().collect();
                prop_assert_eq!(stale, expected);
            }
        }
    }
}
