#![expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]

use super::*;
use tempfile::TempDir;

#[test]
fn record_then_lookup_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    assert!(load_at(&path).unwrap().is_empty());
    record_at(&path, "claude-1", "icm-aaa").unwrap();
    record_at(&path, "claude-2", "icm-bbb").unwrap();
    let map = load_at(&path).unwrap();
    assert_eq!(map.get("claude-1").map(String::as_str), Some("icm-aaa"));
    assert_eq!(map.get("claude-2").map(String::as_str), Some("icm-bbb"));
    assert_eq!(map.get("missing"), None);
}

#[test]
fn record_at_caps_correlation_map_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    for i in 0..MAX_CORRELATION_ENTRIES + 10 {
        record_at(&path, &format!("claude-{i:05}"), "icm-x").unwrap();
    }
    let map = load_at(&path).unwrap();
    assert_eq!(map.len(), MAX_CORRELATION_ENTRIES);
}

#[test]
fn load_at_errors_on_corrupt_json_instead_of_reading_as_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    std::fs::write(&path, "{not valid json").unwrap();
    let result = lookup_session_at(&path, "claude-1");
    assert!(result.is_err(), "corrupt must not be a miss: {result:?}");
    let text = format!("{:#}", result.unwrap_err());
    assert!(text.contains(&path.display().to_string()), "{text}");
}

proptest! {
    /// Any content that is not a JSON map is kept byte for byte in the aside file,
    /// and the write that moved it still records its pair (#2658).
    #[test]
    fn a_corrupt_map_is_kept_byte_for_byte_in_the_aside_file(junk in "#[ -~]{1,64}") {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        std::fs::write(&path, &junk).unwrap();
        record_at(&path, "claude-new", "icm-new").unwrap();
        let aside = dir.path().join("transcript-sessions.json.corrupt");
        prop_assert_eq!(std::fs::read_to_string(&aside).unwrap(), junk);
        let found = lookup_session_at(&path, "claude-new").unwrap();
        prop_assert_eq!(found.as_deref(), Some("icm-new"));
    }
}

/// Write-only mode makes the read fail while a write still succeeds. Before the fix the record
/// wrote back an empty map and erased both stored mappings (#2612).
#[cfg(unix)]
#[test]
fn a_read_error_does_not_erase_the_stored_map() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    record_at(&path, "a", "icm-a").unwrap();
    record_at(&path, "b", "icm-b").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
    let result = record_at(&path, "c", "icm-c");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(result.is_err(), "an unreadable map must fail the record");
    assert_eq!(
        lookup_session_at(&path, "a").unwrap().as_deref(),
        Some("icm-a")
    );
    assert_eq!(
        lookup_session_at(&path, "b").unwrap().as_deref(),
        Some("icm-b")
    );
}

#[cfg(unix)]
#[test]
fn a_lookup_on_an_unreadable_map_is_an_error_not_a_miss() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    record_at(&path, "a", "icm-a").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
    let result = lookup_session_at(&path, "a");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    // A miss would start a new transcript that can never be linked (#2658).
    assert!(
        result.is_err(),
        "an unreadable map must not look like a miss"
    );
}

#[test]
fn a_corrupt_map_is_moved_aside_before_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    std::fs::write(&path, "{not valid json").unwrap();
    record_at(&path, "claude-new", "icm-new").unwrap();

    let aside = dir.path().join("transcript-sessions.json.corrupt");
    assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{not valid json");
    assert_eq!(
        lookup_session_at(&path, "claude-new").unwrap().as_deref(),
        Some("icm-new")
    );
    // The agent must hear that open sessions lost their links (#2834).
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains(&aside.display().to_string()), "{notices}");
}

#[test]
fn a_new_key_that_sorts_first_survives_a_full_map() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    for i in 0..MAX_CORRELATION_ENTRIES {
        record_at(&path, &format!("claude-{i:05}"), "icm-old").unwrap();
    }
    // "aaa" sorts before every "claude-" key, so pop_first would have dropped it at once.
    record_at(&path, "aaa", "icm-new").unwrap();
    assert_eq!(
        lookup_session_at(&path, "aaa").unwrap().as_deref(),
        Some("icm-new")
    );
    assert_eq!(load_at(&path).unwrap().len(), MAX_CORRELATION_ENTRIES);
}

#[test]
fn record_fails_while_another_writer_holds_the_map_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript-sessions.json");
    let _held =
        crate::hook_run::session_state::lock_state_file(dir.path(), MAP_LOCK_KEY, "test holder")
            .unwrap();
    let err = record_at(&path, "claude-1", "icm-a").unwrap_err();
    assert!(
        format!("{err:#}").contains("cannot lock the session map"),
        "{err:#}"
    );
    assert!(!path.exists(), "a write under a busy lock must not happen");
}

#[test]
fn a_bare_file_name_is_refused_before_anything_is_created() {
    // A bare name has no parent. The lock must not fall back to the working directory.
    let err = record_at(Path::new("transcript-sessions.json"), "claude-1", "icm-a").unwrap_err();
    assert!(
        format!("{err:#}").contains("has no parent directory"),
        "{err:#}"
    );
    assert!(!Path::new("transcript-sessions.json").exists());
    assert!(!Path::new("transcript-sessions.lock").exists());
}

use proptest::prelude::*;
proptest! {
    #[test]
    fn record_then_load_returns_every_pair_recorded(
        pairs in prop::collection::vec(("\\PC{1,16}", "\\PC{0,16}"), 0..8),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        let mut expected = BTreeMap::new();
        for (claude, icm) in &pairs {
            record_at(&path, claude, icm).unwrap();
            expected.insert(claude.clone(), icm.clone());
        }
        prop_assert_eq!(load_at(&path).unwrap(), expected);
    }

    #[test]
    fn state_file_is_owner_only(id in "[a-z0-9-]{1,16}") {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        record_at(&path, &id, "icm-x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            prop_assert_eq!(mode & 0o077, 0);
        }
    }
}

#[test]
fn an_aside_name_is_free_only_when_nothing_is_there() {
    let dir = TempDir::new().expect("test");
    let aside = dir.path().join("transcript-sessions.json.corrupt");
    assert!(aside_name_is_free(&aside).unwrap());
    std::fs::write(&aside, "old").expect("test");
    assert!(!aside_name_is_free(&aside).unwrap());
}

#[test]
fn an_aside_name_that_cannot_be_checked_is_an_error_not_a_free_name() {
    let dir = TempDir::new().expect("test");
    let file = dir.path().join("regular");
    std::fs::write(&file, "x").expect("test");
    // A path under a regular file fails with ENOTDIR, so the name cannot be checked.
    assert!(aside_name_is_free(&file.join("x.corrupt")).is_err());
}
