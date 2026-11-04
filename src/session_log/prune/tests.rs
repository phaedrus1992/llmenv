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

// #3348 review: a directory named like a rotation fails `remove_file`. The error must list
// exactly the files that were removed before that failure.
#[test]
fn a_partial_failure_lists_exactly_the_removed_files() {
    use proptest::prelude::*;
    proptest!(|(files in 0u32..4, dirs in 0u32..3)| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&path, 3);
        let mut expect = Vec::new();
        for n in 1..=files {
            let rotation = rotation_path(&path, n);
            write_bytes(&rotation, 3);
            expect.push(rotation);
        }
        for n in files + 1..=files + dirs {
            std::fs::create_dir(rotation_path(&path, n)).unwrap();
        }

        let result = prune_rotations(&path, Duration::ZERO, false, AgeSource::MtimeOnly);

        prop_assert_eq!(result.is_err(), dirs > 0);
        let mut removed = match result {
            Ok(removed) => removed,
            Err(err) => err.removed,
        };
        removed.sort();
        expect.sort();
        prop_assert_eq!(removed, expect.clone());
        for rotation in &expect {
            prop_assert!(!rotation.exists(), "{} was not removed", rotation.display());
        }
    });
}

// #3351: a rotation whose name is not UTF-8 must still match and be pruned. Its base is not
// UTF-8 either, because llmenv writes only ASCII digits after the base.
#[test]
fn is_rotation_of_matches_a_non_utf8_base_by_its_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let base = OsStr::from_bytes(b"\xffsession");
    assert!(is_rotation_of(OsStr::from_bytes(b"\xffsession.3"), base));
    assert!(!is_rotation_of(OsStr::from_bytes(b"\xffsession.x"), base));
    assert!(!is_rotation_of(OsStr::from_bytes(b"\xffsession."), base));
}

// #3335: a rotation is pruned exactly when its mtime is past retention. The offset of half a
// minute keeps the age off the cutoff, so the comparison's edge does not decide the case.
#[test]
fn a_rotation_is_pruned_exactly_when_its_mtime_is_past_retention() {
    use proptest::prelude::*;
    proptest!(|(minutes in 0u64..(48 * 60))| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&path, 3);
        let rotation = rotation_path(&path, 1);
        write_bytes(&rotation, 3);
        let now = SystemTime::now();
        let age = Duration::from_secs(minutes * 60 + 30);
        let mtime = filetime::FileTime::from_system_time(now - age);
        filetime::set_file_mtime(&rotation, mtime).unwrap();
        let retention = Duration::from_secs(24 * 3600);

        let removed =
            prune_rotations_at(&path, retention, false, AgeSource::MtimeOnly, now).unwrap();

        prop_assert_eq!(removed.contains(&rotation), age > retention);
    });
}

// #3335: a name is a rotation only when a dot and digits follow the base. Any other suffix is not.
#[test]
fn is_rotation_of_accepts_only_a_dot_and_digits_after_the_base() {
    use proptest::prelude::*;
    let base = std::ffi::OsStr::new("session-log.jsonl");
    proptest!(|(digits in "[0-9]{1,8}", junk in "[a-z0-9._-]{1,8}")| {
        prop_assume!(!junk.bytes().all(|b| b.is_ascii_digit()));
        let rotated = format!("session-log.jsonl.{digits}");
        let suffixed = format!("session-log.jsonl.{digits}{junk}");
        prop_assert!(is_rotation_of(std::ffi::OsStr::new(&rotated), base));
        prop_assert!(!is_rotation_of(std::ffi::OsStr::new(&suffixed), base));
    });
}

// #3351 review: the byte comparison holds for any base, not only the bases that a test names.
#[test]
fn is_rotation_of_matches_any_byte_base_with_a_digit_suffix() {
    use proptest::prelude::*;
    use std::os::unix::ffi::OsStrExt;
    proptest!(|(
        base in proptest::collection::vec(any::<u8>(), 1..24),
        digits in "[0-9]{1,6}",
        junk in "[a-z]{1,4}",
    )| {
        let base_os = std::ffi::OsStr::from_bytes(&base);
        let mut rotated = base.clone();
        rotated.extend_from_slice(format!(".{digits}").as_bytes());
        prop_assert!(is_rotation_of(std::ffi::OsStr::from_bytes(&rotated), base_os));
        let mut suffixed = base.clone();
        suffixed.extend_from_slice(format!(".{digits}{junk}").as_bytes());
        prop_assert!(!is_rotation_of(std::ffi::OsStr::from_bytes(&suffixed), base_os));
    });
}
