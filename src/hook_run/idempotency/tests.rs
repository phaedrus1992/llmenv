use super::*;
use proptest::prelude::*;

#[test]
fn a_busy_seen_set_lock_is_an_agent_notice_and_the_call_runs_again() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path().join("idempotency");
    let _held = lock_state_file(&store, &state_key("req"), "test holder").unwrap();
    let guard = Guard::new(Some(dir.path()), "req", "id-1").unwrap();
    // A busy lock reads as "not seen", so the guarded call runs again.
    assert!(!guard.already_done());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains(LABEL), "{notices}");
}

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
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("is corrupt, so it started empty"),
        "{notices}"
    );
    store.record("sess", "second");
    assert!(store.contains("sess", "second"));
}

#[test]
fn a_seen_set_cut_inside_a_utf8_character_is_corrupt_and_is_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = SeenStore::new(dir.path());
    store.record("cut", "before");
    std::fs::write(store.path("cut"), b"[\"\xc3").unwrap();
    assert_eq!(store.read("cut"), Some(VecDeque::new()));
    store.record("cut", "after");
    assert!(store.contains("cut", "after"));
}

#[test]
fn a_seen_set_that_cannot_be_read_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = SeenStore::new(dir.path());
    // A directory where the file belongs: reading it fails, and the error is not "missing".
    std::fs::create_dir_all(store.path("sess")).unwrap();
    assert!(store.read("sess").is_none());
    store.record("sess", "abc");
    assert!(
        store.path("sess").is_dir(),
        "an unreadable set is left alone"
    );
    assert!(!store.contains("sess", "abc"));
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("cannot read the idempotency seen-set"),
        "{notices}"
    );
    assert!(notices.contains("Fix:"), "{notices}");
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

proptest! {
    /// The stored set matches a model: each id once, in arrival order, and only the newest
    /// MAX_IDS kept.
    #[test]
    fn the_stored_set_keeps_unique_ids_in_order_up_to_the_cap(
        ids in prop::collection::vec("[a-z]{1,3}", 0..80),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenStore::new(dir.path());
        let mut model: VecDeque<String> = VecDeque::new();
        for id in &ids {
            store.record("m", id);
            if !model.contains(id) {
                model.push_back(id.clone());
                while model.len() > MAX_IDS {
                    model.pop_front();
                }
            }
        }
        let stored = store.read("m").unwrap();
        prop_assert_eq!(stored, model);
    }

    /// Any strict prefix of a saved set, the empty file and a cut UTF-8 character included,
    /// reads as an empty set, never as a different set.
    #[test]
    fn any_cut_off_set_reads_as_empty(
        ids in prop::collection::vec("[a-zé]{1,4}", 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenStore::new(dir.path());
        for id in &ids {
            store.record("c", id);
        }
        let saved = std::fs::read(store.path("c")).unwrap();
        let len = cut.index(saved.len());
        std::fs::write(store.path("c"), &saved[..len]).unwrap();
        prop_assert_eq!(store.read("c"), Some(VecDeque::new()));
    }
}
