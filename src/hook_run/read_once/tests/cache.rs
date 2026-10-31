//! Tests for the session cache of the read-once hook.

use super::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn session_cache_prune_stale_entries() {
    let state_dir = TempDir::new().expect("test");
    let ro_dir = read_once_state_dir(state_dir.path());
    fs::create_dir_all(&ro_dir).expect("test");

    let mut cache = SessionCache::new("test-prune");
    let now = unix_now();
    cache.entries.insert(
        "fresh_file".to_string(),
        ReadEntry {
            path: "fresh_path".to_string(),
            mtime_unix: 1000,
            first_read_at: now,
            hits: 0,
            tokens_saved: 0,
        },
    );
    cache.entries.insert(
        "stale_file".to_string(),
        ReadEntry {
            path: "stale_path".to_string(),
            mtime_unix: 1000,
            first_read_at: now - 3600, // 1 hour ago
            hits: 0,
            tokens_saved: 0,
        },
    );

    cache.prune(60); // 60 second TTL
    assert!(cache.entries.contains_key("fresh_file"));
    assert!(!cache.entries.contains_key("stale_file"));
}

#[test]
fn session_cache_save_and_load_roundtrip() {
    let state_dir = TempDir::new().expect("test");
    let ro_dir = read_once_state_dir(state_dir.path());
    fs::create_dir_all(&ro_dir).expect("test");

    let mut cache = SessionCache::new("test-rt");
    cache.entries.insert(
        "/foo/bar.rs".to_string(),
        ReadEntry {
            path: "/foo/bar.rs".to_string(),
            mtime_unix: 12345,
            first_read_at: unix_now() - 100, // 100 seconds ago — well within 3600s TTL
            hits: 2,
            tokens_saved: 500,
        },
    );
    cache.save(state_dir.path()).expect("test");

    let loaded = SessionCache::load(state_dir.path(), "test-rt", 3600);
    assert_eq!(loaded.session_id, "test-rt");
    assert_eq!(loaded.entries.len(), 1);
    let entry = loaded.entries.get("/foo/bar.rs").expect("test");
    assert_eq!(entry.hits, 2);
    assert_eq!(entry.tokens_saved, 500);
}

// #1196: save() must create its state directory owner-only, even though
// it no longer calls create_dir_all itself -- write_owner_only_atomic's
// own parent-directory creation must be the one and only path.
#[cfg(unix)]
#[test]
fn session_cache_save_creates_state_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = TempDir::new().expect("test");
    let cache = SessionCache::new("test-mode");
    cache.save(state_dir.path()).expect("test");

    let ro_dir = read_once_state_dir(state_dir.path());
    let mode = fs::metadata(&ro_dir).expect("test").permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "read_once dir must be owner-only, got {mode:o}"
    );
}

// #792: ReadEntry and SessionCache derive Serialize/Deserialize and persist
// as JSON. A serde roundtrip must be lossless — a drifted derive (renamed
// field, wrong rename attr) would silently corrupt a user's session cache.
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_read_entry() -> impl Strategy<Value = ReadEntry> {
        (
            ".{0,40}",
            any::<i64>(),
            any::<i64>(),
            any::<u64>(),
            any::<u64>(),
        )
            .prop_map(
                |(path, mtime_unix, first_read_at, hits, tokens_saved)| ReadEntry {
                    path,
                    mtime_unix,
                    first_read_at,
                    hits,
                    tokens_saved,
                },
            )
    }

    fn arb_session_cache() -> impl Strategy<Value = SessionCache> {
        (
            ".{0,20}",
            proptest::collection::hash_map(".{0,30}", arb_read_entry(), 0..5),
        )
            .prop_map(|(session_id, entries)| SessionCache {
                session_id,
                entries,
                read_failed: false,
            })
    }

    proptest! {
        #[test]
        fn read_entry_json_roundtrips(entry in arb_read_entry()) {
            let json = serde_json::to_string(&entry).unwrap();
            let back: ReadEntry = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, entry);
        }

        #[test]
        fn session_cache_json_roundtrips(cache in arb_session_cache()) {
            let json = serde_json::to_string(&cache).unwrap();
            let back: SessionCache = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, cache);
        }
    }
}
