//! The fold of a stranded transcript folder into the durable store, by file descriptor (#1066).

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Move `src` into `dst` recursively, keeping whichever file is newer.
///
/// Renames rather than copies: the stranded folders and the durable store both
/// live under the cache dir, so this is a metadata operation instead of a bulk
/// copy. That matters — a real machine had 1 GiB of transcripts stranded across
/// 27 folders, and this runs from `export`, which fires on every shell prompt.
/// Falls back to copy-then-delete when `rename` can't be used (cross-device).
///
/// Symlinked *entries* are skipped rather than followed — there's no TOCTOU-safe
/// way to follow one into a bounded tree, matching the discipline in
/// `adapter::claude_code::copy_dir_owner_only`.
///
/// A source file older than its durable copy is a stale copy of the same transcript, so it is
/// removed. The result lists the entries left in `src`: links, other kinds of entry, and files
/// as old as their copy, so a caller can name what blocks the folder.
///
/// The two roots are opened once, and every folder below them is reached through the open
/// folder that holds it (#1066, #2949). So a folder swapped for a symlink during the walk cannot
/// redirect it.
pub(super) fn move_dir_newest_wins(src: &Path, dst: &Path) -> anyhow::Result<Vec<PathBuf>> {
    // A link at `dst` is refused before the owner-only create, which also refuses it (#3137).
    anyhow::ensure!(
        !std::fs::symlink_metadata(dst).is_ok_and(|md| md.file_type().is_symlink()),
        "durable transcript dir {} is not a real directory",
        dst.display()
    );
    crate::adapter::skills::create_dir_owner_only(dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    // `create_dir_owner_only`/`create_dir_all` is a no-op (success) when `dst`
    // already resolves through a symlink to an existing directory — it never
    // lstats first. Without this check every following `rename`/`copy` in
    // this call would land inside the symlink's target instead of the
    // intended tree (#1065).
    anyhow::ensure!(
        std::fs::symlink_metadata(dst).is_ok_and(|md| md.file_type().is_dir()),
        "durable transcript dir {} is not a real directory",
        dst.display()
    );
    // #1066: both ends are opened once and every entry below is reached
    // through those descriptors. `src`/`dst` continue downward for error
    // messages only — the kernel never re-resolves them, so a directory
    // swapped for a symlink after the checks above cannot redirect the move.
    use std::os::fd::AsFd as _;
    let src_dir = crate::paths::dirfd::open_dir_nofollow(src)
        .with_context(|| format!("reading {}", src.display()))?;
    let dst_dir = crate::paths::dirfd::open_dir_nofollow(dst)
        .with_context(|| format!("opening {}", dst.display()))?;
    let mut skipped = Vec::new();
    move_dir_at(src_dir.as_fd(), dst_dir.as_fd(), src, dst, &mut skipped)?;
    Ok(skipped)
}

/// [`move_dir_newest_wins`] for two open folders. `src` and `dst` name them in messages only.
fn move_dir_at(
    src_dir: std::os::fd::BorrowedFd<'_>,
    dst_dir: std::os::fd::BorrowedFd<'_>,
    src: &Path,
    dst: &Path,
    skipped: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    use std::os::fd::AsFd as _;
    let entries = crate::paths::dirfd::read_dir_entries(src_dir)
        .with_context(|| format!("reading entry in {}", src.display()))?;
    for entry in entries {
        let from = src.join(&entry.name);
        let to = dst.join(&entry.name);
        if entry.is_dir() && !entry.is_symlink() {
            let sub_src = crate::paths::dirfd::open_dir_at(src_dir, &entry.name)
                .with_context(|| format!("reading {}", from.display()))?;
            let sub_dst = open_or_create_dir_at(dst_dir, &entry.name)
                .with_context(|| format!("opening {}", to.display()))?;
            move_dir_at(sub_src.as_fd(), sub_dst.as_fd(), &from, &to, skipped)?;
            continue;
        }
        if !entry.is_file() || entry.is_symlink() {
            tracing::debug!(
                "inherit: skipping {}, which is not a plain file",
                from.display()
            );
            skipped.push(from);
            continue;
        }
        match age_order_at(src_dir, dst_dir, &entry.name, &from) {
            Some(std::cmp::Ordering::Greater) => {
                move_file_at(src_dir, dst_dir, &entry.name, &from, &to)?;
            }
            // Only a regular file at the destination is a copy of the same transcript. A
            // link there says nothing about the source, which must stay.
            Some(std::cmp::Ordering::Less)
                if crate::paths::dirfd::is_regular_file_at(dst_dir, &entry.name)
                    .unwrap_or(false) =>
            {
                crate::paths::dirfd::remove_file_at(src_dir, &entry.name).with_context(|| {
                    format!("removing {}, which is older than its copy", from.display())
                })?;
            }
            Some(std::cmp::Ordering::Less) => skipped.push(from),
            Some(std::cmp::Ordering::Equal) | None => skipped.push(from),
        }
    }
    Ok(())
}

/// Open the folder `name` in `dir`, owner-only, and create it first when it is absent. The open
/// refuses a symlink, so a link at the destination is never written through (#1065).
fn open_or_create_dir_at(
    dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    match rustix::fs::mkdirat(dir, name, rustix::fs::Mode::from(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    crate::paths::dirfd::open_dir_at(dir, name)
}

/// Rename `from` onto `to`, falling back to copy-then-delete across devices.
/// Move `name` from one open directory to another, newest-wins already decided.
///
/// `from`/`to` are for error messages only. `renameat` is tried first and the
/// copy fallback exists for the cross-filesystem (`EXDEV`) case, which is why
/// the durable store being on a different mount doesn't break inheritance.
fn move_file_at(
    src_dir: std::os::fd::BorrowedFd<'_>,
    dst_dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    from: &Path,
    to: &Path,
) -> anyhow::Result<()> {
    use crate::paths::dirfd;

    match dirfd::rename_at(src_dir, name, dst_dir, name) {
        Ok(()) => return Ok(()),
        // Only a move across file systems needs the copy. Any other error is the real one.
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {}
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("moving {} -> {}", from.display(), to.display()));
        }
    }
    // Owner-only, matching everything else llmenv writes into the durable
    // store; the replace semantics (unlink, then create) are what keep a
    // symlink at the destination from being written through (#1065). The copy
    // streams, because a transcript can be hundreds of MiB.
    dirfd::copy_file_at(src_dir, dst_dir, name, rustix::fs::Mode::from(0o600))
        .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    if let Err(e) = dirfd::remove_file_at(src_dir, name) {
        // `tracing` warnings are off at the default log level, and the duplicate stays.
        tracing::error!(
            "inherit: copied {} to {}, but could not remove the source, so both copies exist: {e}",
            from.display(),
            to.display()
        );
    }
    Ok(())
}

/// How the age of `name` in `src_dir` compares to the same name in `dst_dir`: `Greater` when the
/// source is newer or has no copy. A copy whose age cannot be read counts as older, so the source
/// wins. `None` for a source whose age cannot be read, which stays. Each read error is logged with
/// `from`, the source path.
fn age_order_at(
    src_dir: std::os::fd::BorrowedFd<'_>,
    dst_dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    from: &Path,
) -> Option<std::cmp::Ordering> {
    let src = match crate::paths::dirfd::mtime_at(src_dir, name) {
        Ok(Some(src)) => src,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!(
                "inherit: cannot read the age of {}, so it is not migrated: {e}",
                from.display()
            );
            return None;
        }
    };
    match crate::paths::dirfd::mtime_at(dst_dir, name) {
        Ok(Some(dst)) => Some(src.cmp(&dst)),
        Ok(None) => Some(std::cmp::Ordering::Greater),
        Err(e) => {
            tracing::error!(
                "inherit: cannot read the age of the copy of {}, so the source wins: {e}",
                from.display()
            );
            Some(std::cmp::Ordering::Greater)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    // #1065: `copy_replacing`'s whole contract is "never write through a
    // symlink at `to`" — a plain `std::fs::copy` would follow the symlink
    // and overwrite whatever it points at instead of replacing the symlink
    // itself.
    //
    // #1066: both properties now belong to the fd-relative helpers — the
    // path-based `copy_replacing`/`is_newer` they were written against are
    // gone, replaced by `copy_file_at`/`is_newer_at`, so the tests move with
    // them rather than being deleted alongside the code.
    #[cfg(unix)]
    #[test]
    fn moving_a_file_replaces_a_symlink_at_the_destination() {
        use std::os::fd::AsFd as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        write(&src.join("f"), "new-content");
        let victim = tmp.path().join("victim");
        write(&victim, "must-not-be-touched");
        std::os::unix::fs::symlink(&victim, dst.join("f")).unwrap();

        let src_dir = crate::paths::dirfd::open_dir_nofollow(&src).unwrap();
        let dst_dir = crate::paths::dirfd::open_dir_nofollow(&dst).unwrap();
        move_file_at(
            src_dir.as_fd(),
            dst_dir.as_fd(),
            std::ffi::OsStr::new("f"),
            &src.join("f"),
            &dst.join("f"),
        )
        .unwrap();

        assert!(
            !std::fs::symlink_metadata(dst.join("f"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink at the destination must be replaced, not written through"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("f")).unwrap(),
            "new-content"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "must-not-be-touched",
            "the symlink's target must be untouched"
        );
    }

    // #1065: following a symlink at the destination for the mtime comparison
    // would read the *target's* mtime — an old target makes a planted symlink
    // look "safe to clobber" and selects the vulnerable move path.
    #[cfg(unix)]
    #[test]
    fn is_newer_at_does_not_follow_a_symlink_at_destination() {
        use std::os::fd::AsFd as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        write(&src.join("f"), "content");
        let old_target = tmp.path().join("old-target");
        write(&old_target, "old");
        filetime::set_file_mtime(&old_target, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        std::os::unix::fs::symlink(&old_target, dst.join("f")).unwrap();

        let src_dir = crate::paths::dirfd::open_dir_nofollow(&src).unwrap();
        let dst_dir = crate::paths::dirfd::open_dir_nofollow(&dst).unwrap();
        assert_ne!(
            age_order_at(
                src_dir.as_fd(),
                dst_dir.as_fd(),
                std::ffi::OsStr::new("f"),
                &src.join("f")
            ),
            Some(std::cmp::Ordering::Greater),
            "a symlink at the destination is compared by its own metadata, not its target's"
        );
        // The source is older than the link, but a link is no copy, so the source stays.
        let skipped = move_dir_newest_wins(&src, &dst).unwrap();
        assert_eq!(skipped, [src.join("f")]);
        assert_eq!(std::fs::read_to_string(src.join("f")).unwrap(), "content");
    }

    // #1065: `create_dir_owner_only`/`create_dir_all` succeeds (no-op) when
    // `dst` already resolves through a symlink to an existing directory —
    // it never lstats first. `move_dir_newest_wins` must refuse to proceed
    // rather than silently folding into the symlink's target.
    /// The walk below the roots goes through the open folders, so a root path that now names
    /// a symlink to another tree does not change what is moved (#2949).
    #[cfg(unix)]
    #[test]
    fn the_walk_does_not_resolve_a_path_below_the_open_roots() {
        use std::os::fd::AsFd as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        write(&src.join("sub").join("real.jsonl"), "real");
        write(
            &tmp.path().join("decoy").join("sub").join("decoy.jsonl"),
            "decoy",
        );
        std::fs::create_dir_all(&dst).unwrap();
        let src_dir = crate::paths::dirfd::open_dir_nofollow(&src).unwrap();
        let dst_dir = crate::paths::dirfd::open_dir_nofollow(&dst).unwrap();
        std::fs::rename(&src, tmp.path().join("src-moved")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("decoy"), &src).unwrap();

        move_dir_at(
            src_dir.as_fd(),
            dst_dir.as_fd(),
            &src,
            &dst,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("sub").join("real.jsonl")).unwrap(),
            "real"
        );
        assert!(!dst.join("sub").join("decoy.jsonl").exists());
        assert!(
            tmp.path().join("decoy/sub/decoy.jsonl").exists(),
            "the decoy is untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_a_destination_subfolder_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        write(&src.join("sub").join("a.jsonl"), "a");
        std::fs::create_dir_all(&dst).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dst.join("sub")).unwrap();
        assert!(move_dir_newest_wins(&src, &dst).is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn move_dir_newest_wins_rejects_a_symlinked_destination() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        write(&src.join("a.jsonl"), "content");
        let real_other_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&real_other_dir).unwrap();
        let dst = tmp.path().join("dst-symlink");
        std::os::unix::fs::symlink(&real_other_dir, &dst).unwrap();

        let err = move_dir_newest_wins(&src, &dst).expect_err(
            "a destination that resolves through a symlink must be rejected, not folded into",
        );
        assert!(err.to_string().contains("not a real directory"));
        assert!(
            std::fs::read_dir(&real_other_dir).unwrap().next().is_none(),
            "nothing must have been written into the symlink's target"
        );
    }

    /// A source older than its regular copy is a stale copy, so it goes, and nothing is left.
    #[test]
    fn an_older_source_with_a_newer_copy_is_removed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        write(&src.join("s.jsonl"), "old");
        write(&dst.join("s.jsonl"), "new");
        filetime::set_file_mtime(
            src.join("s.jsonl"),
            filetime::FileTime::from_unix_time(1, 0),
        )
        .unwrap();
        let skipped = move_dir_newest_wins(&src, &dst).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(!src.join("s.jsonl").exists());
        assert_eq!(std::fs::read_to_string(dst.join("s.jsonl")).unwrap(), "new");
    }

    /// Only a move across file systems falls back to a copy. Another rename error, such as a
    /// folder in the way, is the error that the fold returns.
    #[test]
    fn a_rename_error_other_than_a_device_change_is_returned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        write(&src.join("x"), "file");
        write(&dst.join("x").join("inner"), "dir");
        filetime::set_file_mtime(dst.join("x"), filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let err = move_dir_newest_wins(&src, &dst).unwrap_err();
        assert!(format!("{err:#}").contains("moving"), "{err:#}");
        assert_eq!(std::fs::read_to_string(src.join("x")).unwrap(), "file");
    }
}
