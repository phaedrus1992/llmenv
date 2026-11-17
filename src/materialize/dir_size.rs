//! On-disk size of a cache or temp tree, used by the status line and the temp sweep (#3038).

use std::path::Path;

/// Recursively sum the on-disk size of every entry under `path`.
///
/// `prune`'s `removed` list holds directory (or symlink) paths, not files —
/// stat'ing the directory entry itself would report a few KiB regardless of
/// the tree's actual contents, so this walks in and sums real file sizes.
/// Symlinks are sized by their own on-disk length rather than followed,
/// matching `prune`'s own care not to traverse through them. Unreadable
/// entries are skipped (best-effort estimate, not exact accounting).
///
/// # Errors
/// Fails when `path` itself cannot be stat-ed or listed. A zero there would look like a real
/// measurement, so the failure is returned instead (#3038). An entry below `path` that cannot be
/// read is logged and left out of the total, so one unreadable subtree does not drop the whole
/// estimate.
pub(crate) fn dir_size(path: &Path) -> anyhow::Result<DirSize> {
    let mut size = DirSize::default();
    // Not following links keeps a symlink sized by its own length, and stops the walk from
    // leaving the tree. The root link is not followed either, because walkdir follows it by default.
    for step in walkdir::WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
    {
        match step {
            Ok(entry) if entry.file_type().is_dir() => {}
            Ok(entry) => match entry.metadata() {
                Ok(meta) => size.bytes = size.bytes.saturating_add(meta.len()),
                Err(e) => {
                    skip_entry(&e, path)?;
                    size.skipped += 1;
                }
            },
            // A missing root has no size. A missing entry below it was removed during the walk,
            // so it is left out and counted, the same as an unreadable one (#3069).
            Err(e) if e.depth() == 0 && is_not_found(&e) => {
                // An absent root is normal before first use, so it is logged at debug, not warn.
                tracing::debug!("{} does not exist, so its size is zero", path.display());
                return Ok(DirSize::default());
            }
            Err(e) => {
                skip_entry(&e, path)?;
                size.skipped += 1;
            }
        }
    }
    Ok(size)
}

/// The size of a tree, and how many of its entries were left out of the total (#3069).
///
/// A non-zero `skipped` means `bytes` is a floor, not an exact figure. Callers show it as one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirSize {
    pub(crate) bytes: u64,
    pub(crate) skipped: u64,
}

impl DirSize {
    /// Add two sizes. The result is partial when either input is partial.
    pub(crate) fn combine(self, other: DirSize) -> DirSize {
        DirSize {
            bytes: self.bytes.saturating_add(other.bytes),
            skipped: self.skipped.saturating_add(other.skipped),
        }
    }
}

/// Fails for the root, because a zero there would look like a real measurement (#3038). Logs and
/// skips any entry below it, so one unreadable subtree does not drop the whole estimate.
fn skip_entry(err: &walkdir::Error, root: &Path) -> anyhow::Result<()> {
    if err.depth() == 0 {
        anyhow::bail!(
            "cannot read {} for the size estimate: {err}",
            root.display()
        );
    }
    if !is_not_found(err) {
        // The status line renders on every prompt. The first cause of a run is logged at info,
        // so the user who looks finds it. The others stay at debug to keep each render quiet
        // (#3110).
        if FIRST_CAUSE_LOGGED.replace(true) {
            tracing::debug!("{err}; that entry is left out of the size estimate");
        } else {
            tracing::info!("{err}; that entry is left out of the size estimate");
        }
    }
    Ok(())
}

thread_local! {
    /// True once this run has logged the first cause of an entry that was left out. A run is one
    /// process, and the status line renders on one thread.
    static FIRST_CAUSE_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn is_not_found(err: &walkdir::Error) -> bool {
    err.io_error()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(all(test, unix))]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    proptest::proptest! {
        #[test]
        fn combine_is_commutative_associative_and_has_a_zero_identity(
            (a, b, c) in (
                (proptest::prelude::any::<u64>(), 0u64..1000),
                (proptest::prelude::any::<u64>(), 0u64..1000),
                (proptest::prelude::any::<u64>(), 0u64..1000),
            ),
        ) {
            let size = |(bytes, skipped): (u64, u64)| DirSize { bytes, skipped };
            let (a, b, c) = (size(a), size(b), size(c));
            proptest::prop_assert_eq!(a.combine(b), b.combine(a));
            proptest::prop_assert_eq!(a.combine(b).combine(c), a.combine(b.combine(c)));
            proptest::prop_assert_eq!(a.combine(DirSize::default()), a);
        }
    }

    #[test]
    fn a_missing_root_is_zero_and_is_logged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent");
        let mut got = None;
        let logs = crate::test_log_capture::capture_logs(|| got = Some(dir_size(&missing)));
        let size = got
            .expect("dir_size ran")
            .expect("a missing root is not an error");
        assert_eq!(size, DirSize::default());
        assert!(logs.contains("does not exist"), "{logs}");
    }

    #[test]
    fn an_unreadable_subdirectory_is_left_out_and_its_siblings_still_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("readable"), [0u8; 10]).expect("write fixture");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("create locked dir");
        std::fs::write(locked.join("hidden"), [0u8; 99]).expect("write hidden fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the directory");

        let total = dir_size(dir.path());

        // Restore access so the tempdir can be removed, whatever the assertion said.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("unlock the directory");
        let size = total.expect("the root is readable");
        assert_eq!(size.bytes, 10);
        assert_eq!(
            size.skipped, 1,
            "the locked subdirectory is counted as skipped"
        );
    }

    #[test]
    fn the_first_cause_of_a_run_is_logged_at_info_and_the_next_ones_at_debug() {
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = ["first", "second"].map(|name| dir.path().join(name));
        for path in &locked {
            std::fs::create_dir(path).expect("create locked dir");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))
                .expect("lock the directory");
        }
        FIRST_CAUSE_LOGGED.set(false);

        let mut got = None;
        let logs = crate::test_log_capture::capture_logs(|| got = Some(dir_size(dir.path())));

        for path in &locked {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("unlock the directory");
        }
        assert_eq!(
            got.expect("dir_size ran")
                .expect("root is readable")
                .skipped,
            2
        );
        assert_eq!(
            logs.lines().filter(|l| l.contains("INFO")).count(),
            1,
            "{logs}"
        );
        assert_eq!(
            logs.lines().filter(|l| l.contains("DEBUG")).count(),
            1,
            "{logs}"
        );
        assert!(
            logs.contains("Permission denied"),
            "the cause is named: {logs}"
        );
    }

    #[test]
    fn a_root_symlink_counts_its_own_length_and_its_target_is_not_walked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("create target dir");
        std::fs::write(target.join("big"), [0u8; 99]).expect("write target fixture");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        let link_len = std::fs::symlink_metadata(&link).expect("stat link").len();

        let total = dir_size(&link).expect("the link is readable").bytes;

        assert_eq!(total, link_len);
    }

    #[test]
    fn a_symlink_counts_its_own_length_and_its_target_is_not_walked_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("create target dir");
        std::fs::write(target.join("big"), [0u8; 99]).expect("write target fixture");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        let link_len = std::fs::symlink_metadata(&link).expect("stat link").len();

        let total = dir_size(dir.path()).expect("the root is readable").bytes;

        assert_eq!(total, 99 + link_len);
    }
}
