//! Tests of the prune of rotated session-log files.

use super::*;
use std::fs::File;
use std::time::{Duration, SystemTime};

use crate::materialize::cache::AgeSource;
use crate::session_log::rotation::rotation_path;

fn write_bytes(path: &Path, len: u64) {
    std::fs::write(path, vec![b'x'; usize::try_from(len).unwrap()]).unwrap();
}

#[test]
fn prune_removes_old_rotations_and_keeps_the_active_file_and_other_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    write_bytes(&path, 3);
    let old_rotation = rotation_path(&path, 2);
    write_bytes(&old_rotation, 3);
    let old_time = SystemTime::now() - Duration::from_secs(30 * 86_400);
    File::options()
        .write(true)
        .open(&old_rotation)
        .unwrap()
        .set_modified(old_time)
        .unwrap();
    let unrelated = dir.path().join("session-log.jsonl.bak");
    write_bytes(&unrelated, 3);
    File::options()
        .write(true)
        .open(&unrelated)
        .unwrap()
        .set_modified(old_time)
        .unwrap();

    let removed = prune_rotations(
        &path,
        Duration::from_secs(7 * 86_400),
        false,
        AgeSource::MtimeOnly,
    )
    .unwrap();

    assert_eq!(removed, vec![old_rotation.clone()]);
    assert!(!old_rotation.exists());
    assert!(path.exists());
    assert!(
        unrelated.exists(),
        "a name without a numeric suffix is not a rotation"
    );
}

#[test]
fn a_dry_run_lists_an_old_rotation_and_removes_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let old_rotation = rotation_path(&path, 1);
    write_bytes(&old_rotation, 3);
    File::options()
        .write(true)
        .open(&old_rotation)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(30 * 86_400))
        .unwrap();

    let listed = prune_rotations(
        &path,
        Duration::from_secs(7 * 86_400),
        true,
        AgeSource::MtimeOnly,
    )
    .unwrap();

    assert_eq!(listed, vec![old_rotation.clone()]);
    assert!(old_rotation.exists());
}

#[test]
fn a_dry_run_lists_exactly_what_a_real_run_removes() {
    use proptest::prelude::*;
    proptest!(ProptestConfig::with_cases(24), |(ages in proptest::collection::vec(any::<bool>(), 0..6))| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&path, 3);
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        for (i, is_old) in ages.iter().enumerate() {
            let rotation = rotation_path(&path, u32::try_from(i + 1).unwrap());
            write_bytes(&rotation, 3);
            if *is_old {
                File::options().write(true).open(&rotation).unwrap().set_modified(old).unwrap();
            }
        }
        let week = Duration::from_secs(7 * 86_400);

        let mut listed = prune_rotations(&path, week, true, AgeSource::MtimeOnly).unwrap();
        for (i, _) in ages.iter().enumerate() {
            prop_assert!(rotation_path(&path, u32::try_from(i + 1).unwrap()).exists());
        }
        let mut removed = prune_rotations(&path, week, false, AgeSource::MtimeOnly).unwrap();

        listed.sort();
        removed.sort();
        prop_assert_eq!(listed, removed);
        prop_assert!(path.exists(), "the active file was removed");
    });
}

#[test]
fn the_locked_pass_keeps_a_file_that_a_rotation_made_new_and_skips_a_gone_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let renewed = rotation_path(&path, 2);
    write_bytes(&renewed, 3);
    let gone = rotation_path(&path, 3);
    let mut failures = Vec::new();

    let removed = remove_under_lock(
        &path,
        vec![renewed.clone(), gone],
        Expiry {
            now: SystemTime::now(),
            older_than: Duration::from_secs(7 * 86_400),
            age: AgeSource::MtimeOnly,
        },
        &mut failures,
    )
    .unwrap();

    assert!(removed.is_empty(), "{removed:?}");
    assert!(failures.is_empty(), "{failures:?}");
    assert!(renewed.exists(), "a file that is no longer old was removed");
}

#[test]
fn prune_keeps_a_recent_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    write_bytes(&rotation_path(&path, 1), 3);
    assert!(
        prune_rotations(
            &path,
            Duration::from_secs(7 * 86_400),
            false,
            AgeSource::MtimeOnly
        )
        .unwrap()
        .is_empty()
    );
    assert!(rotation_path(&path, 1).exists());
}

// A generation restored with an old mtime keeps its change time, which is now, so it stays.
#[test]
fn a_restored_rotation_is_kept_by_its_change_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    write_bytes(&path, 3);
    let rotation = rotation_path(&path, 2);
    write_bytes(&rotation, 3);
    File::options()
        .write(true)
        .open(&rotation)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(30 * 86_400))
        .unwrap();

    let removed = prune_rotations(
        &path,
        Duration::from_secs(7 * 86_400),
        false,
        AgeSource::MtimeOrCtime,
    )
    .unwrap();

    assert!(removed.is_empty());
    assert!(rotation.exists());
}

// A rotation name that is a folder cannot be removed with `remove_file`, so it fails the pass
// after the file before it was removed. The error must still name that removed file.
#[test]
fn a_partial_prune_error_keeps_the_files_it_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let removable = rotation_path(&path, 1);
    write_bytes(&removable, 3);
    std::fs::create_dir(rotation_path(&path, 2)).unwrap();

    let err = prune_rotations(&path, Duration::ZERO, false, AgeSource::MtimeOnly).unwrap_err();

    assert_eq!(err.removed, vec![removable.clone()]);
    assert!(!removable.exists());
    assert!(format!("{err}").contains("1 removed"), "{err}");
}

#[test]
fn prune_in_a_missing_directory_removes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent").join("session-log.jsonl");
    assert!(
        prune_rotations(&path, Duration::ZERO, false, AgeSource::MtimeOnly)
            .unwrap()
            .is_empty()
    );
}
