//! Tests for `StatusData::load`, the reader side of the status file.

use super::*;
use proptest::prelude::*;

#[test]
fn load_parses_full_example() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmenv-status.json");
    std::fs::write(
        &path,
        r#"{
            "$schema": "llmenv-status-v1", "v": 1, "ts": "2026-07-15T14:23:00Z",
            "scopes": { "tags": ["dev", "rust"] },
            "plugins": { "total": 12, "errors": 0 },
            "mcps": { "total": 12, "errors": 0 },
            "icm": { "memories": 142, "concepts": 47 },
            "throttle": null,
            "config_stale": false,
            "cache": { "prunable_bytes": 15728640 },
            "session_log": 8
        }"#,
    )
    .unwrap();
    let data = StatusData::load(&path);
    assert_eq!(data.scopes.unwrap().tags, vec!["dev", "rust"]);
    assert_eq!(data.plugins.unwrap().total, 12);
    assert_eq!(data.icm.unwrap().memories, 142);
    assert_eq!(data.config_stale, Some(false));
    assert_eq!(data.cache.unwrap().prunable_bytes, Some(15_728_640));
    assert_eq!(data.session_log, Some(8));
    assert!(data.throttle.is_none());
}

#[test]
fn load_missing_file_returns_default() {
    let data = StatusData::load(std::path::Path::new("/nonexistent/llmenv-status.json"));
    assert_eq!(data, StatusData::default());
}

#[test]
fn a_status_file_cannot_set_the_unreadable_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmenv-status.json");
    std::fs::write(&path, br#"{"unreadable": true}"#).unwrap();
    assert!(!StatusData::load(&path).unreadable);
}

#[test]
fn load_corrupt_file_returns_default_marked_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmenv-status.json");
    std::fs::write(&path, b"{ not valid json").unwrap();
    let data = StatusData::load(&path);
    assert_eq!(
        data,
        StatusData {
            unreadable: true,
            ..StatusData::default()
        }
    );
}

#[test]
fn load_partial_json_defaults_missing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmenv-status.json");
    std::fs::write(&path, r#"{"session_log": 3}"#).unwrap();
    let data = StatusData::load(&path);
    assert_eq!(data.session_log, Some(3));
    assert!(data.scopes.is_none());
    assert!(data.plugins.is_none());
}

#[test]
fn load_reads_a_file_that_has_no_throttle_conflict_key() {
    // A status file written before `throttle_conflict` existed must still load.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmenv-status.json");
    std::fs::write(&path, r#"{"throttle_error": "parsing throttle state: /x"}"#).unwrap();
    let data = StatusData::load(&path);
    assert_eq!(
        data.throttle_error.as_deref(),
        Some("parsing throttle state: /x")
    );
    assert_eq!(data.throttle_conflict, None);
}

fn arb_status_data() -> impl Strategy<Value = StatusData> {
    (
        proptest::option::of(proptest::collection::vec("[a-z]{1,8}", 0..4)),
        proptest::option::of((any::<u64>(), any::<u64>())),
        proptest::option::of(any::<bool>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(("[a-z]{1,8}", any::<u64>())),
        proptest::option::of("[ -~]{0,20}"),
        proptest::option::of("[ -~]{0,20}"),
    )
        .prop_map(
            |(
                tags,
                plugins,
                config_stale,
                session_log,
                throttle,
                throttle_error,
                throttle_conflict,
            )| StatusData {
                scopes: tags.map(|tags| ScopesData { tags }),
                plugins: plugins.map(|(total, errors)| CountData { total, errors }),
                mcps: None,
                icm: None,
                throttle: throttle.map(|(backend, cooldown_secs)| ThrottleData {
                    backend,
                    cooldown_secs,
                }),
                throttle_error,
                throttle_conflict,
                config_stale,
                cache: None,
                session_log,
                tasks: None,
                unreadable: false,
            },
        )
}

proptest! {
    #[test]
    fn load_roundtrips_arbitrary_serialized_status_data(data in arb_status_data()) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        let json = serde_json::to_vec(&data).unwrap();
        std::fs::write(&path, json).unwrap();
        prop_assert_eq!(StatusData::load(&path), data);
    }

    #[test]
    fn load_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, &bytes).unwrap();
        // Must degrade to default, never panic, on arbitrary (likely
        // non-JSON) bytes.
        let _ = StatusData::load(&path);
    }
}
