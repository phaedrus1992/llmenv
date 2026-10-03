//! Request ids and the seen-set that keep a detached recorder from storing the same thing twice
//! (#2397). Design: docs/design/issue-2397-detached-idempotency.md
//!
//! The parent derives the id from the event, so a re-spawn or a resumed checkpoint (#2396) sends
//! the same id. The child checks the seen-set before its ICM call and records the id after the
//! call succeeds, so a failed call can be retried. ICM accepts no request id yet, so a duplicate
//! from a lost response stays possible until it does.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::session_ledger::hash_prefix;
use super::session_state::{lock_state_file, prune_orphan_locks, prune_stale_json_files};

/// Ids kept per key. The oldest id goes first.
const MAX_IDS: usize = 1000;
const STALE_DAYS: u64 = 7;
const LABEL: &str = "idempotency seen-set";

/// A stable id for one event: the first 16 hex characters of the SHA-256 over `parts`. Each part
/// is length-prefixed, so `["ab", "c"]` and `["a", "bc"]` differ.
#[must_use]
pub(crate) fn request_id(parts: &[&str]) -> String {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part.as_bytes());
    }
    hash_prefix(&bytes)
}

/// The seen-set files under `state_dir/idempotency/<key>.json`.
#[derive(Debug, Clone)]
pub(crate) struct SeenStore {
    dir: PathBuf,
}

impl SeenStore {
    fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("idempotency"),
        }
    }

    /// Whether `id` was recorded under `key`. A busy lock, an unsafe `key`, or an unreadable
    /// file reads as "not seen": the call then runs, which is the safe side for a lost record.
    fn contains(&self, key: &str, id: &str) -> bool {
        let Some(_lock) = lock_state_file(&self.dir, key, LABEL) else {
            return false;
        };
        self.read(key)
            .is_some_and(|ids| ids.iter().any(|i| i == id))
    }

    /// Record `id` under `key`, dropping the oldest id past [`MAX_IDS`]. Fail-soft: the call that
    /// the id guards has already succeeded, so a failed write is logged and dropped.
    fn record(&self, key: &str, id: &str) {
        let Some(_lock) = lock_state_file(&self.dir, key, LABEL) else {
            return;
        };
        // A corrupt file starts a fresh set; an unreadable one is left alone.
        let Some(mut ids) = self.read(key) else {
            return;
        };
        if ids.iter().any(|i| i == id) {
            return;
        }
        ids.push_back(id.to_string());
        while ids.len() > MAX_IDS {
            ids.pop_front();
        }
        prune_stale_json_files(&self.dir, STALE_DAYS);
        prune_orphan_locks(&self.dir, STALE_DAYS);
        let path = self.path(key);
        let result = serde_json::to_vec(&ids)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::paths::write_owner_only_atomic(&path, &bytes));
        if let Err(e) = result {
            tracing::error!("cannot save {LABEL} {}: {e}", path.display());
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// The stored ids: empty for a missing or corrupt file, `None` for a file that exists but
    /// cannot be read.
    fn read(&self, key: &str) -> Option<VecDeque<String>> {
        let path = self.path(key);
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::error!("{LABEL} {} is corrupt, starting empty: {e}", path.display());
                VecDeque::new()
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(VecDeque::new()),
            Err(e) => {
                tracing::error!("cannot read {LABEL} {}: {e}", path.display());
                None
            }
        }
    }
}

/// A key that is always safe as a file name: `raw` itself when it is a valid short name, else a
/// hash of it. A transcript id from ICM has no promised character set.
#[must_use]
fn state_key(raw: &str) -> String {
    if crate::paths::is_valid_short_name(raw) {
        raw.to_string()
    } else {
        hash_prefix(raw.as_bytes())
    }
}

/// The idempotency check for one detached call: skip the call when its id is already recorded,
/// and record the id once the call has succeeded.
#[derive(Debug)]
pub(crate) struct Guard {
    store: SeenStore,
    key: String,
    id: String,
}

impl Guard {
    /// `None` when there is nothing to guard with: no state dir, or an empty id (a payload from
    /// an older llmenv). The call then runs unguarded.
    #[must_use]
    pub(crate) fn new(state_dir: Option<&Path>, key: &str, id: &str) -> Option<Self> {
        if id.is_empty() {
            return None;
        }
        Some(Self {
            store: SeenStore::new(state_dir?),
            key: state_key(key),
            id: id.to_string(),
        })
    }

    /// Whether the call already succeeded for this id.
    pub(crate) fn already_done(&self) -> bool {
        let done = self.store.contains(&self.key, &self.id);
        if done {
            tracing::debug!("request {} already recorded, skipped", self.id);
        }
        done
    }

    /// Mark the call as done. Call only after it succeeded.
    pub(crate) fn done(&self) {
        self.store.record(&self.key, &self.id);
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn request_id_is_stable_hex_and_sensitive_to_every_part() {
        let id = request_id(&["s", "icm-store", "t1", "body"]);
        assert_eq!(id, request_id(&["s", "icm-store", "t1", "body"]));
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        for changed in [
            ["x", "icm-store", "t1", "body"],
            ["s", "other", "t1", "body"],
            ["s", "icm-store", "t2", "body"],
            ["s", "icm-store", "t1", "body2"],
        ] {
            assert_ne!(id, request_id(&changed), "{changed:?}");
        }
    }

    #[test]
    fn request_id_does_not_alias_part_boundaries() {
        assert_ne!(request_id(&["ab", "c"]), request_id(&["a", "bc"]));
        assert_ne!(request_id(&["a", ""]), request_id(&["", "a"]));
    }

    #[test]
    fn missing_file_is_empty_then_record_then_contains() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenStore::new(dir.path());
        assert!(!store.contains("sess", "abc"));
        store.record("sess", "abc");
        assert!(store.contains("sess", "abc"));
        assert!(!store.contains("other", "abc"), "keys are separate files");
    }

    #[test]
    fn past_the_cap_the_oldest_id_goes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenStore::new(dir.path());
        for i in 0..=MAX_IDS {
            store.record("sess", &format!("id{i}"));
        }
        assert!(!store.contains("sess", "id0"));
        assert!(store.contains("sess", "id1"));
        assert!(store.contains("sess", &format!("id{MAX_IDS}")));
    }

    #[test]
    fn corrupt_file_reads_as_empty_and_is_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenStore::new(dir.path());
        store.record("sess", "first");
        std::fs::write(store.path("sess"), "{ not json").unwrap();
        assert!(!store.contains("sess", "first"));
        store.record("sess", "second");
        assert!(store.contains("sess", "second"));
    }

    #[test]
    fn unsafe_key_is_never_seen_and_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenStore::new(dir.path());
        store.record("../escape", "abc");
        assert!(!store.contains("../escape", "abc"));
        assert!(!dir.path().join("escape.json").exists());
    }

    #[test]
    fn guard_skips_a_recorded_id_and_records_only_after_done() {
        let dir = tempfile::tempdir().unwrap();
        let guard = Guard::new(Some(dir.path()), "s", "id").unwrap();
        assert!(!guard.already_done());
        assert!(!guard.already_done(), "no record until done() is called");
        guard.done();
        assert!(guard.already_done());
        let other = Guard::new(Some(dir.path()), "s", "other").unwrap();
        assert!(!other.already_done());
    }

    #[test]
    fn guard_is_absent_without_a_state_dir_or_an_id() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Guard::new(None, "s", "id").is_none());
        assert!(Guard::new(Some(dir.path()), "s", "").is_none());
    }

    #[test]
    fn state_key_keeps_a_safe_name_and_hashes_anything_else() {
        assert_eq!(state_key("abc-123"), "abc-123");
        let hashed = state_key("../etc/passwd");
        assert_eq!(hashed.len(), 16);
        assert!(crate::paths::is_valid_short_name(&hashed));
        assert_eq!(state_key(""), state_key(""));
    }

    proptest! {
        #[test]
        fn state_key_is_idempotent_and_always_a_valid_file_name(raw in ".{0,40}") {
            let key = state_key(&raw);
            prop_assert!(crate::paths::is_valid_short_name(&key), "{key:?}");
            prop_assert_eq!(state_key(&key), key);
        }

        #[test]
        fn distinct_part_lists_give_distinct_ids(
            lists in prop::collection::hash_set(
                prop::collection::vec("[a-z0-9]{0,4}", 1..4), 2..200,
            ),
        ) {
            let mut seen = std::collections::HashSet::new();
            for list in &lists {
                let parts: Vec<&str> = list.iter().map(String::as_str).collect();
                prop_assert!(seen.insert(request_id(&parts)), "collision for {list:?}");
            }
        }

        #[test]
        fn the_set_never_exceeds_the_cap(extra in 0usize..40) {
            let dir = tempfile::tempdir().unwrap();
            let store = SeenStore::new(dir.path());
            // Seed the file directly so the property does not run 1000 locked writes.
            let ids: VecDeque<String> = (0..MAX_IDS).map(|i| format!("seed{i}")).collect();
            std::fs::create_dir_all(&store.dir).unwrap();
            std::fs::write(store.path("k"), serde_json::to_vec(&ids).unwrap()).unwrap();
            for i in 0..extra {
                store.record("k", &format!("new{i}"));
            }
            prop_assert!(store.read("k").unwrap().len() <= MAX_IDS);
        }
    }
}
