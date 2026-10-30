//! Finds a path that another user owns inside a tree that a sweep could not remove (#3184).
//!
//! A sweep runs as the user, so it cannot remove a file that root or another user owns in a
//! folder it cannot change. Such a file stops the removal part-way, and the error alone does not
//! say which file, or what to do.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The result of a search for a path of another owner.
#[derive(Debug, PartialEq, Eq)]
enum Search {
    /// The first path, in sorted order, that another user owns, and that user's uid.
    Found(PathBuf, u32),
    /// Every path in the tree has this owner.
    Clean,
    /// No path of another owner was found, but this path could not be checked, for this reason.
    Unknown(PathBuf, String),
}

/// The cause text for a failed removal of the tree at `p`. When a path in the tree has another
/// owner, the text names that path, its owner, and the fix. When part of the tree cannot be
/// checked, the text says so.
pub(super) fn removal_cause(p: &Path, e: &std::io::Error) -> String {
    #[cfg(unix)]
    {
        let me = rustix::process::geteuid().as_raw();
        match search(p, me, &stat, &list) {
            Search::Found(path, owner) => {
                return format!(
                    "{e}. {} is owned by uid {owner}, not by this user (uid {me}), so the sweep \
                     cannot remove it. Fix: remove {} as uid {owner}, for example with sudo, \
                     then run `llmenv clean` again",
                    path.display(),
                    p.display()
                );
            }
            Search::Unknown(path, why) => {
                return format!(
                    "{e}. The owner of {} could not be checked: {why}. Fix: check the \
                     permissions under {}, or remove it by hand",
                    path.display(),
                    p.display()
                );
            }
            Search::Clean => {}
        }
    }
    e.to_string()
}

/// The owner uid of `p`, and whether it is a folder. A symlink is judged as itself.
#[cfg(unix)]
fn stat(p: &Path) -> std::io::Result<(u32, bool)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).map(|meta| (meta.uid(), meta.is_dir()))
}

/// The paths in the folder `p`.
#[cfg(unix)]
fn list(p: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::read_dir(p)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect()
}

/// Search the tree at `p` for the first path, in sorted order, that a user other than `me` owns.
/// A path that is gone is skipped: the removal can delete part of the tree while it fails.
fn search(
    p: &Path,
    me: u32,
    stat: &impl Fn(&Path) -> std::io::Result<(u32, bool)>,
    list: &impl Fn(&Path) -> std::io::Result<Vec<PathBuf>>,
) -> Search {
    let (owner, is_dir) = match stat(p) {
        Ok(found) => found,
        Err(e) if e.kind() == ErrorKind::NotFound => return Search::Clean,
        Err(e) => return Search::Unknown(p.to_path_buf(), e.to_string()),
    };
    if owner != me {
        return Search::Found(p.to_path_buf(), owner);
    }
    if !is_dir {
        return Search::Clean;
    }
    let mut children = match list(p) {
        Ok(children) => children,
        Err(e) if e.kind() == ErrorKind::NotFound => return Search::Clean,
        Err(e) => return Search::Unknown(p.to_path_buf(), e.to_string()),
    };
    children.sort();
    // A path of another owner later in the tree tells the user more than an unreadable one.
    let mut unknown = None;
    for child in children {
        match search(&child, me, stat, list) {
            found @ Search::Found(..) => return found,
            Search::Unknown(path, why) => {
                unknown.get_or_insert(Search::Unknown(path, why));
            }
            Search::Clean => {}
        }
    }
    unknown.unwrap_or(Search::Clean)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    const ME: u32 = 501;

    /// A tree as a map from path to (owner, folder). A key that `unreadable` names fails.
    struct Model {
        tree: BTreeMap<PathBuf, (u32, bool)>,
        unreadable: Option<PathBuf>,
    }

    impl Model {
        fn stat(&self, p: &Path) -> std::io::Result<(u32, bool)> {
            if self.unreadable.as_deref() == Some(p) {
                return Err(ErrorKind::PermissionDenied.into());
            }
            self.tree
                .get(p)
                .copied()
                .ok_or_else(|| ErrorKind::NotFound.into())
        }

        fn list(&self, p: &Path) -> std::io::Result<Vec<PathBuf>> {
            Ok(self
                .tree
                .keys()
                .filter(|k| k.parent() == Some(p))
                .cloned()
                .collect())
        }

        fn search(&self) -> Search {
            search(Path::new("/r"), ME, &|p| self.stat(p), &|p| self.list(p))
        }
    }

    /// A tree under `/r` from relative paths and owners. Every folder above a path is owned by
    /// `ME`, unless the input names it.
    fn model(entries: &[(String, u32)]) -> Model {
        let mut tree = BTreeMap::from([(PathBuf::from("/r"), (ME, true))]);
        for (rel, owner) in entries {
            let path = Path::new("/r").join(rel);
            for folder in path.ancestors().skip(1) {
                if folder.starts_with("/r") {
                    tree.entry(folder.to_path_buf()).or_insert((ME, true));
                }
            }
            let is_dir = tree.get(&path).is_some_and(|(_, d)| *d);
            tree.insert(path, (*owner, is_dir));
        }
        // A path that holds another path is a folder, whatever order the input gave.
        let parents: Vec<PathBuf> = tree
            .keys()
            .filter_map(|k| k.parent())
            .map(Path::to_path_buf)
            .collect();
        for parent in parents {
            if let Some(entry) = tree.get_mut(&parent) {
                entry.1 = true;
            }
        }
        Model {
            tree,
            unreadable: None,
        }
    }

    proptest! {
        /// The answer is the first path in sorted order that another user owns. Sorted path
        /// order is the order of a depth-first walk with sorted children.
        #[test]
        fn the_first_foreign_path_in_sorted_order_is_found(
            entries in proptest::collection::vec(
                ("[ab]{1}(/[ab]{1}){0,2}", prop_oneof![Just(ME), Just(0u32), Just(502u32)]),
                0..10,
            ),
        ) {
            let model = model(&entries);
            let expected = model
                .tree
                .iter()
                .find(|(_, (owner, _))| *owner != ME)
                .map_or(Search::Clean, |(p, (owner, _))| Search::Found(p.clone(), *owner));
            prop_assert_eq!(model.search(), expected);
        }
    }

    #[test]
    fn an_unreadable_subtree_is_unknown_when_nothing_foreign_is_found() {
        let mut model = model(&[("a/x".to_owned(), ME), ("b".to_owned(), ME)]);
        model.unreadable = Some(PathBuf::from("/r/a"));
        assert_eq!(
            model.search(),
            Search::Unknown(
                PathBuf::from("/r/a"),
                ErrorKind::PermissionDenied.to_string()
            )
        );
    }

    #[test]
    fn a_foreign_path_wins_over_an_unreadable_one() {
        let mut model = model(&[("a/x".to_owned(), ME), ("b".to_owned(), 0)]);
        model.unreadable = Some(PathBuf::from("/r/a"));
        assert_eq!(model.search(), Search::Found(PathBuf::from("/r/b"), 0));
    }

    #[test]
    fn a_vanished_root_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let e = std::io::Error::from(ErrorKind::NotFound);
        assert_eq!(removal_cause(&dir.path().join("gone"), &e), e.to_string());
    }

    #[cfg(unix)]
    #[test]
    fn a_tree_that_the_user_owns_gives_the_error_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("f"), b"x").unwrap();
        let e = std::io::Error::from(ErrorKind::PermissionDenied);
        assert_eq!(removal_cause(dir.path(), &e), e.to_string());
    }

    #[cfg(unix)]
    #[test]
    fn a_real_tree_of_another_owner_is_named_with_its_uid() {
        let dir = tempfile::tempdir().unwrap();
        let me = rustix::process::geteuid().as_raw();
        assert_eq!(
            search(dir.path(), me.wrapping_add(1), &stat, &list),
            Search::Found(dir.path().to_path_buf(), me)
        );
    }
}
