//! The newest mtime under a cache folder, for the cache GC age check.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context as _;

use super::skip_vanished;

/// Which timestamp judges the age of a file under a GC candidate.
///
/// On a platform without a status-change time in `std`, `MtimeOrCtime` reads as `MtimeOnly`.
///
/// `MtimeOnly` trusts the modification time alone. A tree unpacked with its old mtimes kept
/// (`tar -x`, `cp -p`) then looks old at once. `MtimeOrCtime` also counts the status-change time,
/// which an unpack sets to now, so the tree reads as used (#3195). Production code builds its
/// rule with `AgeRule::new`, which picks `MtimeOrCtime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgeSource {
    /// Tests age a tree with `set_modified`, which cannot move the change time back, so only they
    /// can name this rule: a release build has no such variant (#3432).
    #[cfg(any(test, feature = "test-support"))]
    MtimeOnly,
    MtimeOrCtime,
}

/// The newest file mtime under `p`, as the `state/tmp` sweep judges age: a folder mtime changes
/// when a child is removed, so a partial delete would reset the age (#3184). On failure, the
/// error is logged with its path, `p` is recorded in `failed`, and `None` is returned, so one
/// unreadable folder does not stop the rest of the pass (#2859). An entry that another process
/// removes during the walk is gone, not a failure, so it is not recorded (#3423).
pub(crate) fn newest_file_mtime_or_record(
    p: &Path,
    source: AgeSource,
    record: impl FnOnce(PathBuf, String),
) -> Option<SystemTime> {
    match newest_file_mtime(p, source) {
        Ok(m) => Some(m),
        Err(e) if super::is_vanished(&e) => {
            tracing::debug!(path = %p.display(), "cache gc: entry removed during the walk");
            None
        }
        Err(e) => {
            tracing::error!(
                "cache gc cannot scan the mtimes under {}, so it is kept: {e:#}. Fix: check \
                 the permissions of that folder, or remove it by hand.",
                p.display()
            );
            record(p.to_path_buf(), format!("{e:#}"));
            None
        }
    }
}

/// The newest mtimes under one folder: of any entry, and of entries that are not folders.
struct Newest {
    any: SystemTime,
    non_dir: Option<SystemTime>,
}

/// Newest age stamp of an entry under `dir` that is not a folder. A folder mtime changes when a
/// child is added or removed, so a partial delete or an unrelated tool would make an old tree look
/// fresh (#3184). A tree that holds no file falls back to its newest folder mtime.
///
/// The cost of `MtimeOnly`: a folder that only gains new empty folders does not look used, and a
/// tree unpacked with its old file mtimes kept looks old. `MtimeOrCtime` removes the second cost.
/// A chmod or chown also sets a new ctime, which only delays removal.
///
/// Under `MtimeOrCtime` a folder also counts its change time, so an unpacked tree of only folders
/// reads as used. A child add or remove already moves the folder mtime, so the change time adds no
/// new reset for a partial delete (#3184).
pub(crate) fn newest_file_mtime(dir: &Path, age: AgeSource) -> anyhow::Result<SystemTime> {
    let newest = walk_from(dir, age)?;
    Ok(newest.non_dir.unwrap_or(newest.any))
}

fn walk_from(dir: &Path, age: AgeSource) -> anyhow::Result<Newest> {
    let meta = dir
        .metadata()
        .with_context(|| format!("stat {}", dir.display()))?;
    let any = file_stamp(&meta, age).with_context(|| format!("stat {}", dir.display()))?;
    let mut newest = Newest { any, non_dir: None };
    walk_mtime(dir, &mut newest, age)?;
    Ok(newest)
}

/// A child that another tool removes during the walk is skipped, not an error (#2835).
fn walk_mtime(dir: &Path, newest: &mut Newest, age: AgeSource) -> anyhow::Result<()> {
    let Some(entries) = skip_vanished(std::fs::read_dir(dir))
        .with_context(|| format!("reading {}", dir.display()))?
    else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry under {}", dir.display()))?;
        let p = entry.path();
        // `DirEntry::metadata` does not follow a symlink, so one stat gives both the type and
        // the mtime. A symlink counts by its own mtime and is never descended, the same as the
        // other GC sweeps (#3076).
        let Some(meta) =
            skip_vanished(entry.metadata()).with_context(|| format!("stat {}", p.display()))?
        else {
            continue;
        };
        let stamp = file_stamp(&meta, age).with_context(|| format!("mtime {}", p.display()))?;
        newest.any = newest.any.max(stamp);
        if meta.is_dir() {
            walk_mtime(&p, newest, age)?;
        } else {
            newest.non_dir = Some(newest.non_dir.map_or(stamp, |n| n.max(stamp)));
        }
    }
    Ok(())
}

/// The time that judges one entry that is not a folder, under `age`.
pub(crate) fn file_stamp(meta: &std::fs::Metadata, age: AgeSource) -> std::io::Result<SystemTime> {
    let modified = meta.modified()?;
    Ok(match age {
        #[cfg(any(test, feature = "test-support"))]
        AgeSource::MtimeOnly => modified,
        AgeSource::MtimeOrCtime => modified.max(change_time(meta)),
    })
}

#[cfg(unix)]
fn change_time(meta: &std::fs::Metadata) -> SystemTime {
    use std::os::unix::fs::MetadataExt as _;
    ctime_to_system_time(meta.ctime(), meta.ctime_nsec())
}

/// The change time in `secs` and `nanos`, as the file system reports it.
///
/// The kernel sets the change time from its clock, so a pre-1970 value is not a real case. It
/// maps to the epoch, the oldest value, so it never keeps a tree alive by accident. A nanosecond
/// part outside one second is read as zero.
#[cfg(unix)]
fn ctime_to_system_time(secs: i64, nanos: i64) -> SystemTime {
    use crate::paths::dirfd::stat_time;
    stat_time(secs, nanos)
        .or_else(|| stat_time(secs, 0))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Windows has no status-change time in `std`. The epoch is the oldest value, so the `max` in
/// `file_stamp` then gives the mtime.
#[cfg(not(unix))]
fn change_time(_meta: &std::fs::Metadata) -> SystemTime {
    SystemTime::UNIX_EPOCH
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs::File;
    use std::time::{Duration, UNIX_EPOCH};

    use proptest::prelude::*;

    use super::*;

    fn set(path: &Path, secs: u64) -> SystemTime {
        let when = UNIX_EPOCH + Duration::from_secs(secs);
        File::open(path).unwrap().set_modified(when).unwrap();
        when
    }

    // The documented cost of the rule (#3195): a new empty folder does not make an old tree new.
    #[test]
    fn a_fresh_empty_folder_in_a_tree_with_old_files_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let old = set(&file, 1_000_000);
        std::fs::create_dir(dir.path().join("lock")).unwrap();
        assert_eq!(
            newest_file_mtime(dir.path(), AgeSource::MtimeOnly).unwrap(),
            old
        );
    }

    // An unpack keeps the old mtime, but the change time is now. The tree must read as used.
    #[test]
    fn an_unpacked_tree_with_old_mtimes_is_fresh_by_its_change_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        set(&file, 1_000_000);
        let got = newest_file_mtime(dir.path(), AgeSource::MtimeOrCtime).unwrap();
        assert!(
            got > SystemTime::now() - Duration::from_secs(60),
            "got {got:?}"
        );
    }

    // An unpack of a tree with only folders keeps the old folder mtimes, but the change time is now.
    #[test]
    fn an_unpacked_folder_only_tree_is_fresh_by_its_change_time() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a");
        std::fs::create_dir(&sub).unwrap();
        let old = set(&sub, 1_000_000);
        set(dir.path(), 1_000_000);
        assert_eq!(
            newest_file_mtime(dir.path(), AgeSource::MtimeOnly).unwrap(),
            old
        );
        let got = newest_file_mtime(dir.path(), AgeSource::MtimeOrCtime).unwrap();
        assert!(
            got > SystemTime::now() - Duration::from_secs(60),
            "got {got:?}"
        );
    }

    // An entry listed by the gc and removed before its walk is gone, not a failure (#3423).
    #[test]
    fn an_entry_removed_before_its_walk_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("removed-after-listing");
        let mut recorded = Vec::new();
        let got = newest_file_mtime_or_record(&gone, AgeSource::MtimeOrCtime, |path, cause| {
            recorded.push((path, cause));
        });
        assert_eq!(got, None);
        assert!(recorded.is_empty(), "{recorded:?}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_entry_is_still_recorded() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut recorded = Vec::new();
        let got = newest_file_mtime_or_record(&locked, AgeSource::MtimeOrCtime, |path, _| {
            recorded.push(path);
        });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(got, None);
        assert_eq!(recorded, vec![locked]);
    }

    #[cfg(unix)]
    #[test]
    fn a_ctime_out_of_range_is_the_epoch() {
        assert_eq!(ctime_to_system_time(-1, 5), UNIX_EPOCH);
        assert_eq!(
            ctime_to_system_time(10, 999_999_999),
            UNIX_EPOCH + Duration::new(10, 999_999_999)
        );
        assert_eq!(
            ctime_to_system_time(10, 1_000_000_000),
            UNIX_EPOCH + Duration::from_secs(10)
        );
        assert_eq!(
            ctime_to_system_time(10, 2_000_000_000),
            UNIX_EPOCH + Duration::from_secs(10)
        );
    }

    proptest! {
        /// Any ctime maps to a time, never a panic (#3280). A negative ctime is the epoch, and a
        /// nanosecond part outside one second is read as zero.
        #[cfg(unix)]
        #[test]
        fn a_ctime_never_panics_and_maps_by_its_parts(
            // Mix the full range with values near the one-second edge, which a uniform i64 misses.
            secs in prop_oneof![any::<i64>(), -10i64..4_000_000_000],
            nanos in prop_oneof![any::<i64>(), -10i64..1_000_000_010, 999_999_990i64..1_000_000_010],
        ) {
            let got = ctime_to_system_time(secs, nanos);
            prop_assert!(got >= UNIX_EPOCH);
            if secs < 0 {
                prop_assert_eq!(got, UNIX_EPOCH);
            } else if (0..1_000_000_000).contains(&nanos) {
                prop_assert_eq!(i64::from(got.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()), nanos);
                prop_assert_eq!(i64::try_from(got.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap(), secs);
            } else {
                prop_assert_eq!(got, ctime_to_system_time(secs, 0));
            }
        }

        /// With any file in the tree, the answer is the newest file mtime, whatever the folder
        /// mtimes are. With no file, it is the newest folder mtime.
        #[test]
        fn the_newest_file_wins_over_every_folder(
            entries in proptest::collection::vec((0usize..3, any::<bool>(), 1u64..1_000_000), 0..8),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let folders = [dir.path().to_path_buf(), dir.path().join("a"), dir.path().join("a/b")];
            std::fs::create_dir_all(&folders[2]).unwrap();
            let mut newest_file = None;
            for (i, (depth, is_file, secs)) in entries.iter().enumerate() {
                let path = folders[*depth].join(format!("e{i}"));
                if *is_file {
                    std::fs::write(&path, b"x").unwrap();
                    let when = set(&path, *secs);
                    newest_file = newest_file.max(Some(when));
                } else {
                    std::fs::create_dir(&path).unwrap();
                }
            }
            // Folder times are set last: creating a child changes its parent's mtime.
            let mut newest_folder = UNIX_EPOCH;
            for (i, folder) in folders.iter().rev().enumerate() {
                newest_folder = newest_folder.max(set(folder, 2_000_000 + i as u64));
            }
            for entry in std::fs::read_dir(&folders[0]).unwrap().chain(std::fs::read_dir(&folders[1]).unwrap()).chain(std::fs::read_dir(&folders[2]).unwrap()) {
                let path = entry.unwrap().path();
                if path.is_dir() && !folders.contains(&path) {
                    newest_folder = newest_folder.max(set(&path, 3_000_000));
                }
            }
            let got = newest_file_mtime(dir.path(), AgeSource::MtimeOnly).unwrap();
            prop_assert_eq!(got, newest_file.unwrap_or(newest_folder));
        }

        /// The change-time rule only adds to the mtime rule: it never reads a tree as older than
        /// the mtime-only rule does, whatever the mtimes are (#3195).
        #[test]
        fn the_change_time_rule_never_reads_a_tree_as_older(
            secs in proptest::collection::vec(1u64..1_000_000, 1..6),
        ) {
            let dir = tempfile::tempdir().unwrap();
            for (i, s) in secs.iter().enumerate() {
                let path = dir.path().join(format!("f{i}"));
                std::fs::write(&path, b"x").unwrap();
                set(&path, *s);
            }
            let only = newest_file_mtime(dir.path(), AgeSource::MtimeOnly).unwrap();
            let both = newest_file_mtime(dir.path(), AgeSource::MtimeOrCtime).unwrap();
            prop_assert!(both >= only, "ctime rule {both:?} is older than mtime rule {only:?}");
        }

        /// A symlink counts by its own mtime and is never descended. A target's mtime, and any file
        /// under a linked folder, never moves the result (#3076).
        #[test]
        fn a_symlink_counts_by_its_own_mtime_never_its_target(
            links in proptest::collection::vec((1u64..1_000_000, 1u64..2_000_000_000), 1..6),
        ) {
            let root = tempfile::tempdir().unwrap();
            let tree = root.path().join("tree");
            let outside = root.path().join("outside");
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            let far = outside.join("far.txt");
            std::fs::write(&far, b"x").unwrap();
            set(&far, 2_000_000_000);
            let folder_link = tree.join("linked-folder");
            std::os::unix::fs::symlink(&outside, &folder_link).unwrap();
            let epoch = filetime::FileTime::from_unix_time(0, 0);
            filetime::set_symlink_file_times(&folder_link, epoch, epoch).unwrap();
            let mut expected = 0;
            for (i, (own, target)) in links.iter().enumerate() {
                let target_path = outside.join(format!("t{i}"));
                std::fs::write(&target_path, b"x").unwrap();
                set(&target_path, *target);
                let link = tree.join(format!("l{i}"));
                std::os::unix::fs::symlink(&target_path, &link).unwrap();
                let at = filetime::FileTime::from_unix_time(i64::try_from(*own).unwrap(), 0);
                filetime::set_symlink_file_times(&link, at, at).unwrap();
                expected = expected.max(*own);
            }

            let got = newest_file_mtime(&tree, AgeSource::MtimeOnly).unwrap();

            prop_assert_eq!(got, std::time::UNIX_EPOCH + std::time::Duration::from_secs(expected));
        }
    }
}
