//! The newest mtime under a cache folder, for the cache GC age check.

use std::path::Path;
use std::time::SystemTime;

use anyhow::Context as _;

use super::skip_vanished;

/// The newest mtimes under one folder: of any entry, and of entries that are not folders.
struct Newest {
    any: SystemTime,
    non_dir: Option<SystemTime>,
}

/// Newest mtime of an entry under `dir` that is not a folder. A folder mtime changes when a child
/// is added or removed, so a partial delete or an unrelated tool would make an old tree look
/// fresh (#3184). A tree that holds no file falls back to its newest folder mtime.
///
/// The cost of this rule: a folder that only gains new empty folders does not look used, and a
/// tree unpacked with its old file mtimes kept (`tar -x`, `cp -p`) looks old. The sweeps keep any
/// entry that a process holds open, so such a tree is at risk only once nothing uses it (#3195).
pub(crate) fn newest_file_mtime(dir: &Path) -> anyhow::Result<SystemTime> {
    let newest = walk_from(dir)?;
    Ok(newest.non_dir.unwrap_or(newest.any))
}

fn walk_from(dir: &Path) -> anyhow::Result<Newest> {
    let any = dir
        .metadata()
        .and_then(|m| m.modified())
        .with_context(|| format!("stat {}", dir.display()))?;
    let mut newest = Newest { any, non_dir: None };
    walk_mtime(dir, &mut newest)?;
    Ok(newest)
}

/// A child that another tool removes during the walk is skipped, not an error (#2835).
fn walk_mtime(dir: &Path, newest: &mut Newest) -> anyhow::Result<()> {
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
        let m = meta
            .modified()
            .with_context(|| format!("mtime {}", p.display()))?;
        newest.any = newest.any.max(m);
        if meta.is_dir() {
            walk_mtime(&p, newest)?;
        } else {
            newest.non_dir = Some(newest.non_dir.map_or(m, |n| n.max(m)));
        }
    }
    Ok(())
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
        assert_eq!(newest_file_mtime(dir.path()).unwrap(), old);
    }

    proptest! {
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
            let got = newest_file_mtime(dir.path()).unwrap();
            prop_assert_eq!(got, newest_file.unwrap_or(newest_folder));
        }
    }
}
