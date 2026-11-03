use super::*;

use crate::session_log::prune::is_rotation_of;

fn write_bytes(path: &Path, len: u64) {
    std::fs::write(path, vec![b'x'; usize::try_from(len).unwrap()]).unwrap();
}

#[test]
fn a_rotation_that_waits_on_the_lock_checks_the_length_again() {
    // Another process rotated while this one waited, so the active file is small again.
    // A second rotation would move that new file into generation 1 (#2831).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    std::fs::write(&path, "x".repeat(20)).unwrap();
    let held = rotation_lock(&path).unwrap();
    let waiter = {
        let path = path.clone();
        std::thread::spawn(move || rotate_if_full(&path, 10))
    };
    // Give the waiter time to pass its first length check and block on the lock.
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::fs::rename(&path, rotation_path(&path, 1)).unwrap();
    std::fs::write(&path, "new").unwrap();
    drop(held);

    assert_eq!(waiter.join().unwrap().unwrap(), None);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    assert!(!rotation_path(&path, 2).exists());
}

#[test]
fn a_file_under_the_cap_is_not_rotated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    write_bytes(&path, 9);
    assert_eq!(rotate_if_full(&path, 10).unwrap(), None);
    assert!(path.exists());
    assert!(!rotation_path(&path, 1).exists());
}

#[test]
fn a_file_at_the_cap_rotates_to_generation_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    write_bytes(&path, 10);
    let rotated = rotate_if_full(&path, 10).unwrap();
    assert_eq!(rotated.as_deref(), Some("session-log.jsonl.1"));
    assert!(!path.exists(), "the active file must move away");
    assert_eq!(
        std::fs::metadata(rotation_path(&path, 1)).unwrap().len(),
        10
    );
}

#[test]
fn rotations_keep_only_the_last_kept_generations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let rounds = KEPT_ROTATIONS + 2;
    for round in 0..rounds {
        // Each round writes a distinct size, so the generations can be told apart.
        write_bytes(&path, 10 + u64::from(round));
        rotate_if_full(&path, 10).unwrap();
    }
    for n in 1..=KEPT_ROTATIONS {
        assert!(
            rotation_path(&path, n).exists(),
            "generation {n} is missing"
        );
    }
    assert!(!rotation_path(&path, KEPT_ROTATIONS + 1).exists());
    // Generation 1 holds the newest rotated file, the last round's.
    let last = 10 + u64::from(rounds - 1);
    assert_eq!(
        std::fs::metadata(rotation_path(&path, 1)).unwrap().len(),
        last
    );
}

#[test]
fn a_missing_active_file_is_not_a_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    assert_eq!(rotate_if_full(&path, 10).unwrap(), None);
}

#[test]
fn the_rotation_marker_names_the_file_the_old_lines_moved_to() {
    let marker = rotation_marker("session-log.jsonl.1");
    let value: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(value["llmenv_log"], "rotated");
    assert_eq!(value["rotated_to"], "session-log.jsonl.1");
}

#[test]
fn the_rotation_marker_is_one_json_line_that_names_its_file() {
    use proptest::prelude::*;
    proptest!(|(name in "(?s).{0,40}")| {
        let marker = rotation_marker(&name);
        prop_assert!(!marker.contains('\n'), "marker spans lines: {marker}");
        let value: serde_json::Value = serde_json::from_str(&marker).unwrap();
        prop_assert_eq!(value["rotated_to"].as_str(), Some(name.as_str()));
    });
}

#[test]
fn a_rotated_name_is_the_base_then_a_dot_then_digits() {
    use proptest::prelude::*;
    use std::ffi::OsStr;
    proptest!(|(digits in "[0-9]{1,6}", letter in "[a-z]{1,4}")| {
        let base = OsStr::new("session-log.jsonl");
        let rotated = format!("session-log.jsonl.{digits}");
        prop_assert!(is_rotation_of(OsStr::new(&rotated), base));
        let with_letters = format!("session-log.jsonl.{letter}{digits}");
        prop_assert!(!is_rotation_of(OsStr::new(&with_letters), base));
        let other_base = format!("other.jsonl.{digits}");
        prop_assert!(!is_rotation_of(OsStr::new(&other_base), base));
    });
}

#[test]
fn the_rotation_lock_is_never_read_as_a_rotated_file() {
    use proptest::prelude::*;
    use std::ffi::OsStr;
    proptest!(|(base in "[a-z.-]{1,12}(\\.[0-9]{1,3})?")| {
        let lock = format!("{base}{LOCK_SUFFIX}");
        prop_assert!(!is_rotation_of(OsStr::new(&lock), OsStr::new(&base)));
    });
}

#[test]
fn a_lock_held_past_the_wait_is_an_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let _held = rotation_lock(&path).unwrap();
    let err = rotation_lock(&path).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock, "{err}");
}
