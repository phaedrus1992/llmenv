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
/// ([`crate::merge::merge_signature`]).
pub fn write(
    cache_root: &Path,
    key: &str,
    bundle_memory: &[Memory],
    bundle_host: &BTreeMap<String, HostEntry>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(cache_root)?;
    let entry = PersistedMergeCache {
        key: key.to_string(),
        bundle_memory: bundle_memory.to_vec(),
        bundle_host: bundle_host.clone(),
    };
    let json = serde_json::to_vec(&entry)?;
    std::fs::write(cache_file(cache_root), json)?;
    Ok(())
}

/// Read the persisted slice iff its stored key matches `key`. Any I/O error,
/// missing file, or malformed content is treated as a cache miss (`None`)
/// rather than an error — this is a pure optimization over the live-merge
/// fallback, so a broken cache file must never block resolution.
#[must_use]
pub fn read_if_matching(
    cache_root: &Path,
    key: &str,
) -> Option<(Vec<Memory>, BTreeMap<String, HostEntry>)> {
    let bytes = std::fs::read(cache_file(cache_root)).ok()?;
    let entry: PersistedMergeCache = serde_json::from_slice(&bytes).ok()?;
    (entry.key == key).then_some((entry.bundle_memory, entry.bundle_host))
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
    }
}
