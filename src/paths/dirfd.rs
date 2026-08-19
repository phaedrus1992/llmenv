//! `openat`-relative directory traversal (#1066).
//!
//! Every recursive tree walk built on `std::fs::*` has the same residual: it
//! `lstat`s a path, approves it, then hands the *same path* to `read_dir` or
//! `copy`, which the kernel re-resolves from scratch. Swap the directory for a
//! symlink in that window and the walk operates on the symlink's target
//! instead of what was approved. The per-entry guards in those walks are
//! correct; the gap is the top-level path being re-resolved between the check
//! and the use, and no amount of re-checking the path closes it — the check
//! and the use have to name the same object, not the same string.
//!
//! A file descriptor is that object. Open a directory once with `O_NOFOLLOW |
//! O_DIRECTORY`, then descend with `openat` relative to that fd: each step
//! either opens the entry that was in the directory the caller already holds,
//! or fails. There is no second resolution to race.
//!
//! Severity is low — winning the race requires being the same user, who can
//! already read the files — so this is defence in depth rather than a
//! vulnerability fix. It's cheap once `rustix` is already a dependency.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat};

/// One entry of an open directory.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) name: OsString,
    /// The entry's own type — from `readdir`'s `d_type` where the filesystem
    /// provides it, otherwise an `fstatat(AT_SYMLINK_NOFOLLOW)`. Either way it
    /// describes the link itself, never its target. Private on purpose:
    /// callers ask `is_dir`/`is_file`/`is_symlink` rather than matching on a
    /// `rustix` enum, so the crate boundary doesn't leak the backing library.
    file_type: FileType,
}

impl Entry {
    pub(crate) fn is_dir(&self) -> bool {
        self.file_type == FileType::Directory
    }

    pub(crate) fn is_symlink(&self) -> bool {
        self.file_type == FileType::Symlink
    }

    pub(crate) fn is_file(&self) -> bool {
        self.file_type == FileType::RegularFile
    }
}

/// Flags shared by every directory open here: never follow a final symlink,
/// and fail rather than open something that isn't a directory.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::DIRECTORY);

/// Open `path` as a directory without following it if it is a symlink.
///
/// # Errors
///
/// Fails when `path` is a symlink (`ELOOP` on Linux, `ENOTDIR` on macOS — the
/// two disagree on which applies when `O_NOFOLLOW` and `O_DIRECTORY` are
/// combined) or isn't a directory, plus the usual `openat` errors. Note that only the *final* component is
/// protected: the parent components are still resolved by the kernel, so this
/// is the entry point to a safe walk, not a safe path resolution.
pub(crate) fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    Ok(openat(rustix::fs::CWD, path, DIR_FLAGS, Mode::empty())?)
}

/// Open `name` — a single component, never a path — as a directory relative to
/// the already-open `dir`, without following a symlink.
///
/// # Errors
///
/// Returns `InvalidInput` when `name` contains a separator or is `.`/`..`:
/// those would re-introduce multi-component resolution, which is the thing
/// this module exists to avoid. Otherwise the `openat` errors above.
pub(crate) fn open_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    reject_traversal(name)?;
    Ok(openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// The entries of an open directory, with `.` and `..` removed.
///
/// # Errors
///
/// `getdents`/`readdir` errors. A directory the caller holds open can still
/// fail to be read (a revoked filesystem, an I/O error), so this is fallible
/// even though the fd is already open.
pub(crate) fn read_dir_entries(dir: BorrowedFd<'_>) -> io::Result<Vec<Entry>> {
    // `Dir::read_from` dups the fd, so the caller's stays usable afterwards
    // and the iteration doesn't disturb its seek position.
    let mut out = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes()).to_owned();
        if name == "." || name == ".." {
            continue;
        }
        // `d_type` is optional: several filesystems (older XFS, some network
        // mounts) report `Unknown` for every entry. Left unresolved that
        // silently turns "is this a directory?" into `false` everywhere and a
        // caller walks nothing, so fall back to an explicit no-follow stat.
        let file_type = match entry.file_type() {
            FileType::Unknown => {
                let st = statat(dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
                FileType::from_raw_mode(st.st_mode as rustix::fs::RawMode)
            }
            known => known,
        };
        out.push(Entry { name, file_type });
    }
    Ok(out)
}

/// Read a regular file named `name` from the already-open `dir`, without
/// following a symlink.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component (see [`open_dir_at`]),
/// and the `openat`/read errors otherwise. A symlink at `name` fails rather
/// than being followed.
pub(crate) fn read_file_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Vec<u8>> {
    use std::io::Read as _;

    reject_traversal(name)?;
    let fd = openat(dir, name, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())?;
    let mut buf = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Rename `old_name` in `old_dir` to `new_name` in `new_dir`, both relative to
/// already-open directories.
///
/// Keeps `rename(2)`'s replace semantics — an existing `new_name`, symlink
/// included, is replaced rather than written through.
///
/// # Errors
///
/// `InvalidInput` for anything but single components, plus `renameat` errors
/// (`EXDEV` when the two directories are on different filesystems, which
/// callers are expected to fall back from).
pub(crate) fn rename_at(
    old_dir: BorrowedFd<'_>,
    old_name: &OsStr,
    new_dir: BorrowedFd<'_>,
    new_name: &OsStr,
) -> io::Result<()> {
    reject_traversal(old_name)?;
    reject_traversal(new_name)?;
    Ok(rustix::fs::renameat(old_dir, old_name, new_dir, new_name)?)
}

/// Unlink `name` from `dir`. A symlink is removed, never followed.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component, plus `unlinkat` errors.
pub(crate) fn remove_file_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    reject_traversal(name)?;
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?)
}

/// The modification time of `name` in `dir`, without following a symlink.
/// `None` when it can't be read at all — a missing entry and an unreadable one
/// are the same answer to "is the source newer".
pub(crate) fn mtime_at(dir: BorrowedFd<'_>, name: &OsStr) -> Option<std::time::SystemTime> {
    if reject_traversal(name).is_err() {
        return None;
    }
    let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    let secs = u64::try_from(st.st_mtime).ok()?;
    let nanos = u32::try_from(st.st_mtime_nsec).ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos))
}

/// Write `bytes` to `name` in `dir`, replacing whatever is there — including a
/// symlink, which is unlinked rather than written through.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component, plus the `openat`/write
/// errors. Created with `mode` so the file is never briefly more permissive
/// than intended.
pub(crate) fn write_file_at(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    bytes: &[u8],
    mode: Mode,
) -> io::Result<()> {
    use std::io::Write as _;

    reject_traversal(name)?;
    // Unlink first, then create exclusively: `O_TRUNC` on an existing symlink
    // would follow it and truncate the target.
    match remove_file_at(dir, name) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let fd = openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
        mode,
    )?;
    std::fs::File::from(fd).write_all(bytes)
}

/// Open `name` as a directory relative to `dir`, creating it (as `mode`) if
/// it doesn't exist yet, without ever following a symlink at `name`.
///
/// Used to walk a multi-component relative path one directory at a time
/// (#1427) — the fd-relative analogue of `create_dir_all`, which resolves
/// every intermediate component by path and so follows a symlink planted at
/// any of them.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component (see [`open_dir_at`]).
/// Fails, rather than following it, when `name` exists as a symlink (or any
/// non-directory). A race where another process creates `name` between the
/// initial open attempt and `mkdirat` is resolved by retrying the open once —
/// `mkdirat`'s own `EEXIST` in that case is not itself an error here.
fn open_or_create_dir_at(dir: BorrowedFd<'_>, name: &OsStr, mode: Mode) -> io::Result<OwnedFd> {
    reject_traversal(name)?;
    match openat(dir, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => return Ok(fd),
        Err(e) if e != rustix::io::Errno::NOENT => return Err(e.into()),
        Err(_) => {}
    }
    if let Err(e) = rustix::fs::mkdirat(dir, name, mode)
        && e != rustix::io::Errno::EXIST
    {
        return Err(e.into());
    }
    Ok(openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// Write `bytes` to `root.join(rel)`, descending through every directory
/// component of `rel` via [`open_or_create_dir_at`] (creating missing ones
/// owner-only) rather than a path-based `create_dir_all`, then replacing the
/// leaf the way [`write_file_at`] does.
///
/// Closes the gap a leaf-only fix (a `write_file_at`/`copy_replacing_symlink`
/// call preceded by `create_dir_all`) leaves open: `create_dir_all` resolves
/// every intermediate component by path and so follows a symlink planted at
/// any of them, landing the write inside the symlink's target — a strictly
/// stronger primitive than writing through the leaf alone (#1427).
///
/// `rel` should be validated with [`super::is_unsafe_join_target`] before
/// calling — both current callers do — but `..`, an absolute path, or an
/// empty/embedded-separator component fails safe here too, as a defense in
/// depth: each component goes through [`open_or_create_dir_at`]'s or
/// [`write_file_at`]'s own `reject_traversal` check, which rejects anything
/// but a single ordinary path component (security-audit, #1427).
///
/// # Errors
///
/// `InvalidInput` when `rel` has no components, or any component isn't a
/// single ordinary path component. Otherwise propagates any open, `mkdir`,
/// or write failure — including a symlinked directory component, which
/// fails the walk rather than being followed.
pub(crate) fn write_file_through_dirs(
    root: &Path,
    rel: &Path,
    bytes: &[u8],
    mode: Mode,
) -> io::Result<()> {
    use std::os::fd::AsFd as _;

    let mut components: Vec<&OsStr> = rel.iter().collect();
    let Some(file_name) = components.pop() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: empty relative path", rel.display()),
        ));
    };

    let mut dir = open_dir_nofollow(root)?;
    for component in components {
        dir = open_or_create_dir_at(dir.as_fd(), component, Mode::from(0o700))?;
    }
    write_file_at(dir.as_fd(), file_name, bytes, mode)
}

/// Remove the empty directory `name` from `dir`. A symlink is never followed:
/// `AT_REMOVEDIR` on one fails rather than removing its target.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component, plus `unlinkat` errors
/// (`ENOTEMPTY` when the directory still has entries).
pub(crate) fn remove_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    reject_traversal(name)?;
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

/// Reject anything that isn't a single, ordinary path component.
fn reject_traversal(name: &OsStr) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || name == "." || name == ".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a single path component"),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn opens_a_real_directory_and_lists_its_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("child")).unwrap();
        std::fs::write(tmp.path().join("file"), b"x").unwrap();

        let dir = open_dir_nofollow(tmp.path()).unwrap();
        let mut names: Vec<_> = read_dir_entries(dir.as_fd())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        assert_eq!(names, ["child", "file"]);
    }

    #[test]
    fn dot_entries_are_not_returned() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            read_dir_entries(open_dir_nofollow(tmp.path()).unwrap().as_fd())
                .unwrap()
                .is_empty(),
            "an empty directory has no entries once . and .. are dropped"
        );
    }

    // The whole point: a symlink to a directory must not open as that
    // directory, however ordinary it looks from a path-based stat.
    #[test]
    fn refuses_to_open_a_symlink_to_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // The errno differs by platform — Linux reports ELOOP, macOS ENOTDIR
        // — so the assertion is on the property that matters (the open fails)
        // rather than on a number that would make this a Linux-only test.
        assert!(
            open_dir_nofollow(&link).is_err(),
            "a symlink must not open as the directory it points at"
        );
    }

    #[test]
    fn refuses_to_open_a_file_as_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(open_dir_nofollow(&file).is_err());
    }

    #[test]
    fn descends_relative_to_an_open_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();

        let root = open_dir_nofollow(tmp.path()).unwrap();
        let a = open_dir_at(root.as_fd(), OsStr::new("a")).unwrap();
        let b = read_dir_entries(a.as_fd()).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name, "b");
        assert!(b[0].is_dir());
    }

    // A relative open must stay inside the directory it was handed. Accepting
    // `..` or an embedded slash would resolve multiple components again and
    // give back the race this module removes.
    #[test]
    fn relative_open_rejects_anything_but_a_single_component() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let root = open_dir_nofollow(tmp.path()).unwrap();

        for bad in ["..", ".", "", "a/b", "/etc"] {
            let err = open_dir_at(root.as_fd(), OsStr::new(bad)).unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidInput,
                "{bad:?} should be rejected before it reaches openat"
            );
        }
    }

    #[test]
    fn entry_types_describe_the_link_not_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

        let dir = open_dir_nofollow(tmp.path()).unwrap();
        let entries = read_dir_entries(dir.as_fd()).unwrap();
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_symlink(), "a symlink to a dir is a symlink entry");
        assert!(!link.is_dir(), "and must not read as a directory");
    }
    #[test]
    fn reads_a_file_relative_to_an_open_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("data"), b"hello").unwrap();
        let dir = open_dir_nofollow(tmp.path()).unwrap();
        assert_eq!(
            read_file_at(dir.as_fd(), OsStr::new("data")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn refuses_to_read_through_a_symlinked_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret"), tmp.path().join("link")).unwrap();
        let dir = open_dir_nofollow(tmp.path()).unwrap();
        assert!(
            read_file_at(dir.as_fd(), OsStr::new("link")).is_err(),
            "reading must not follow a symlink to its target"
        );
    }

    #[test]
    fn file_reads_reject_anything_but_a_single_component() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open_dir_nofollow(tmp.path()).unwrap();
        for bad in ["../etc/passwd", "a/b", ".."] {
            assert_eq!(
                read_file_at(dir.as_fd(), OsStr::new(bad))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
    #[test]
    fn renames_between_open_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::create_dir(tmp.path().join("b")).unwrap();
        std::fs::write(tmp.path().join("a/f"), b"v").unwrap();

        let a = open_dir_nofollow(&tmp.path().join("a")).unwrap();
        let b = open_dir_nofollow(&tmp.path().join("b")).unwrap();
        rename_at(a.as_fd(), OsStr::new("f"), b.as_fd(), OsStr::new("f")).unwrap();

        assert!(!tmp.path().join("a/f").exists());
        assert_eq!(std::fs::read(tmp.path().join("b/f")).unwrap(), b"v");
    }

    // Replacing must remove a symlink, not write through it — the #1065
    // behaviour, preserved now that the write goes through an fd.
    #[test]
    fn writing_replaces_a_symlink_instead_of_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"original").unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join("d/link")).unwrap();

        let d = open_dir_nofollow(&tmp.path().join("d")).unwrap();
        write_file_at(d.as_fd(), OsStr::new("link"), b"new", Mode::from(0o600)).unwrap();

        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"original",
            "the symlink's target must be untouched"
        );
        assert_eq!(std::fs::read(tmp.path().join("d/link")).unwrap(), b"new");
        assert!(
            !std::fs::symlink_metadata(tmp.path().join("d/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn mtime_is_none_for_a_missing_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open_dir_nofollow(tmp.path()).unwrap();
        assert!(mtime_at(dir.as_fd(), OsStr::new("nope")).is_none());
        std::fs::write(tmp.path().join("here"), b"x").unwrap();
        assert!(mtime_at(dir.as_fd(), OsStr::new("here")).is_some());
    }
    // ---- open_or_create_dir_at / write_file_through_dirs (#1427) ----

    #[test]
    fn open_or_create_dir_at_creates_a_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_dir_nofollow(tmp.path()).unwrap();

        let child =
            open_or_create_dir_at(root.as_fd(), OsStr::new("child"), Mode::from(0o700)).unwrap();

        assert!(tmp.path().join("child").is_dir());
        assert_eq!(read_dir_entries(child.as_fd()).unwrap().len(), 0);
    }

    #[test]
    fn open_or_create_dir_at_opens_an_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("child")).unwrap();
        std::fs::write(tmp.path().join("child/marker"), b"x").unwrap();
        let root = open_dir_nofollow(tmp.path()).unwrap();

        let child =
            open_or_create_dir_at(root.as_fd(), OsStr::new("child"), Mode::from(0o700)).unwrap();

        let names: Vec<_> = read_dir_entries(child.as_fd())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["marker"],
            "must open the existing dir, not recreate it"
        );
    }

    /// The whole point: a symlinked directory component must never be
    /// followed, even to create a missing child underneath what looks like
    /// it from a path-based stat (#1427).
    #[test]
    fn open_or_create_dir_at_refuses_a_symlinked_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, tmp.path().join("link")).unwrap();
        let root = open_dir_nofollow(tmp.path()).unwrap();

        assert!(
            open_or_create_dir_at(root.as_fd(), OsStr::new("link"), Mode::from(0o700)).is_err(),
            "a symlinked directory component must not be opened as the directory it points at"
        );
    }

    #[test]
    fn write_file_through_dirs_creates_missing_intermediate_directories() {
        let tmp = tempfile::tempdir().unwrap();

        write_file_through_dirs(
            tmp.path(),
            Path::new("a/b/c.txt"),
            b"payload",
            Mode::from(0o600),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(tmp.path().join("a/b/c.txt")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn write_file_through_dirs_writes_a_single_component_path() {
        let tmp = tempfile::tempdir().unwrap();

        write_file_through_dirs(tmp.path(), Path::new("leaf.txt"), b"x", Mode::from(0o600))
            .unwrap();

        assert_eq!(std::fs::read(tmp.path().join("leaf.txt")).unwrap(), b"x");
    }

    /// The leaf write must replace a pre-existing symlink rather than write
    /// through it — `write_file_at`'s existing contract, exercised here
    /// through the multi-component wrapper (#1427).
    #[test]
    fn write_file_through_dirs_does_not_write_through_a_symlinked_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"must-not-be-touched").unwrap();
        std::os::unix::fs::symlink(&victim, tmp.path().join("a/leaf.txt")).unwrap();

        write_file_through_dirs(
            tmp.path(),
            Path::new("a/leaf.txt"),
            b"new-content",
            Mode::from(0o600),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(tmp.path().join("a/leaf.txt")).unwrap(),
            b"new-content"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"must-not-be-touched");
    }

    /// The core property this issue exists for: a symlinked *intermediate*
    /// directory component must not be followed, however ordinary it looks
    /// from a path-based stat. Planting one where a bundle file's directory
    /// would go must make the write fail rather than land inside the
    /// symlink's target (#1427).
    #[test]
    fn write_file_through_dirs_refuses_a_symlinked_intermediate_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join("a")).unwrap();

        let err = write_file_through_dirs(
            tmp.path(),
            Path::new("a/b/leaf.txt"),
            b"attacker-controlled",
            Mode::from(0o600),
        )
        .unwrap_err();
        let _ = err;

        assert!(
            !outside.join("b/leaf.txt").exists(),
            "the write must not land inside the symlinked directory's target"
        );
    }

    /// Defense in depth (security-audit, #1427): both current callers
    /// pre-validate `rel` with `is_unsafe_join_target`, but this function
    /// must fail safe on its own too — each component goes through
    /// `reject_traversal` internally via `open_or_create_dir_at`/
    /// `write_file_at`, so `..`, an absolute path, or an embedded `..`
    /// never reach a real `openat`/`mkdirat` call.
    #[test]
    fn write_file_through_dirs_rejects_unsafe_components_even_without_caller_validation() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in [
            Path::new("../escape.txt"),
            Path::new("/etc/passwd"),
            Path::new("a/../b.txt"),
        ] {
            assert!(
                write_file_through_dirs(tmp.path(), bad, b"x", Mode::from(0o600)).is_err(),
                "{bad:?} must be rejected even without a caller's own pre-validation"
            );
        }
    }

    #[test]
    fn removes_an_empty_directory_but_not_a_symlink_to_one() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("empty")).unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, tmp.path().join("link")).unwrap();

        let dir = open_dir_nofollow(tmp.path()).unwrap();
        remove_dir_at(dir.as_fd(), OsStr::new("empty")).unwrap();
        assert!(!tmp.path().join("empty").exists());

        assert!(
            remove_dir_at(dir.as_fd(), OsStr::new("link")).is_err(),
            "a symlink must not be removed as if it were the directory it points at"
        );
        assert!(target.exists(), "and its target must survive");
    }
}
