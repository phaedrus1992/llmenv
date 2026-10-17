//! Disk-persisted cache of the bundle-merge's memory/host slice (#920).
//!
//! `hook_run::memory_url` recomputes the full bundle merge on every
//! invocation because each `hook-run` is a fresh subprocess — the in-process
//! `MERGE_CACHE` there never survives across calls. `build_manifest` (the
//! `regenerate`/`export` CLI path) already runs the full merge, so it
//! persists the memory/host slice here, keyed by
//! [`crate::merge::merge_signature`]. `hook_run::memory_url` reads it back
//! instead of redoing the full merge, falling back to a live merge when the
//! artifact is missing or the key doesn't match — a stale read would
//! silently resolve to the wrong ICM memory endpoint, so a key mismatch must
//! always be treated as a miss, never a hit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{HostEntry, Memory};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedMergeCache {
    key: String,
    bundle_memory: Vec<Memory>,
    bundle_host: BTreeMap<String, HostEntry>,
}

fn cache_file(cache_root: &Path) -> PathBuf {
    cache_root.join("merge-cache.json")
}

/// Persist the bundle-only memory/host slice, keyed on `key`
/// ([`crate::merge::merge_signature`]). Written atomically and owner-only
/// (0600), matching every other persisted state artifact in this codebase
/// (`materialize::manifest::CacheManifest`, `icm.rs`, `task/`, `auth/`, …) —
/// the memory/host slice carries the same sensitivity (memory-backend
/// hostnames/ports) as those, and a torn write would otherwise be readable
/// as a false hit or corrupt-but-parseable value by a concurrent `hook-run`.
pub(crate) fn write(
    cache_root: &Path,
    key: &str,
    bundle_memory: &[Memory],
    bundle_host: &BTreeMap<String, HostEntry>,
) -> anyhow::Result<()> {
    let entry = PersistedMergeCache {
        key: key.to_string(),
        bundle_memory: bundle_memory.to_vec(),
        bundle_host: bundle_host.clone(),
    };
    let json = serde_json::to_vec(&entry)?;
    crate::paths::write_owner_only_atomic(&cache_file(cache_root), &json)?;
    Ok(())
}

/// Read the persisted slice iff its stored key matches `key`. A missing file is
/// a quiet miss. A read error or malformed content is logged, noticed, and also
/// a miss: this is a pure optimization over the live-merge fallback, so a broken
/// cache file must never block resolution. It must still be visible, or a
/// permission error rebuilds the merge on every run with no cause shown (#2861).
#[must_use]
pub(crate) fn read_if_matching(
    cache_root: &Path,
    key: &str,
) -> Option<(Vec<Memory>, BTreeMap<String, HostEntry>)> {
    let start = std::time::Instant::now();
    let path = cache_file(cache_root);
    let result = match read_entry(&path) {
        Ok(entry) => entry
            .filter(|entry| entry.key == key)
            .map(|entry| (entry.bundle_memory, entry.bundle_host)),
        Err(e) => {
            note_unreadable(&path, &e);
            None
        }
    };
    crate::cache_trace::emit_cache_trace("merge_sig", result.is_some(), start.elapsed(), None);
    result
}

/// The stored entry. `None` for a missing file.
fn read_entry(path: &Path) -> anyhow::Result<Option<PersistedMergeCache>> {
    use anyhow::Context as _;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let entry = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a valid merge cache", path.display()))?;
    Ok(Some(entry))
}

fn note_unreadable(path: &Path, err: &anyhow::Error) {
    tracing::error!("merge cache unusable, using a live merge: {err:#}");
    crate::agent_notice::record(format!(
        "llmenv: the merge cache is unusable ({err:#}), so each run rebuilds the bundle merge. \
         Fix: delete {} and run `llmenv regenerate`.",
        path.display()
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn memory(host: &str) -> Memory {
        serde_json::from_value(serde_json::json!({
            "server_host": host,
            "port": 1,
        }))
        .unwrap()
    }

    #[test]
    fn write_then_read_matching_key_round_trips() {
        let dir = tempdir().unwrap();
        let mem = vec![memory("h")];
        let mut host = BTreeMap::new();
        host.insert(
            "h".to_string(),
            HostEntry {
                addr: "1.2.3.4".into(),
            },
        );

        write(dir.path(), "key-a", &mem, &host).unwrap();
        let (got_mem, got_host) = read_if_matching(dir.path(), "key-a").expect("expected a hit");

        assert_eq!(got_mem, mem);
        assert_eq!(got_host, host);
    }

    #[test]
    fn read_with_mismatched_key_is_a_miss() {
        let dir = tempdir().unwrap();
        write(dir.path(), "key-a", &[memory("h")], &BTreeMap::new()).unwrap();

        assert!(read_if_matching(dir.path(), "key-b").is_none());
    }

    #[test]
    fn read_with_no_artifact_is_a_miss() {
        let dir = tempdir().unwrap();
        assert!(read_if_matching(dir.path(), "anything").is_none());
    }

    #[test]
    fn read_with_corrupt_artifact_is_a_miss_not_an_error() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(cache_file(dir.path()), b"not json").unwrap();

        assert!(read_if_matching(dir.path(), "anything").is_none());
        assert!(read_entry(&cache_file(dir.path())).is_err());
    }

    #[test]
    fn a_missing_artifact_is_not_an_error() {
        let dir = tempdir().unwrap();
        assert!(read_entry(&cache_file(dir.path())).unwrap().is_none());
    }

    #[test]
    fn an_unreadable_artifact_is_an_error_not_a_miss() {
        // A directory in place of the file fails the read with an error other than NotFound.
        let dir = tempdir().unwrap();
        std::fs::create_dir(cache_file(dir.path())).unwrap();
        let err = read_entry(&cache_file(dir.path())).unwrap_err();
        assert!(format!("{err:#}").contains("merge-cache.json"), "{err:#}");
        assert!(read_if_matching(dir.path(), "anything").is_none());
    }
}
