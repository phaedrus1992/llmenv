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

/// Read a regular file named `name` of at most `max_bytes` bytes from the already-open `dir`,
/// without following a symlink. The open does not block on a FIFO (#2942).
///
/// # Errors
///
/// `InvalidInput` for anything but a single component (see [`open_dir_at`]),
/// and the `openat`/read errors otherwise. A symlink at `name` fails rather
/// than being followed. `InvalidData` when the entry is not a regular file or is too large.
pub(crate) fn read_file_at(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    max_bytes: u64,
) -> io::Result<Vec<u8>> {
    llmenv_paths::read_open_file(open_regular_file_at(dir, name)?, max_bytes)
}

/// Copy the regular file `name` in `src_dir` to `name` in `dst_dir`. The copy replaces whatever
/// is at the destination, a symlink included, which is unlinked rather than written through. The
/// copy streams, so a large file does not have to fit in memory.
///
/// # Errors
///
/// The errors of [`read_file_at`] other than the size, and the `unlinkat`/`openat`/write errors
/// at the destination.
pub(crate) fn copy_file_at(
    src_dir: BorrowedFd<'_>,
    dst_dir: BorrowedFd<'_>,
    name: &OsStr,
    mode: Mode,
) -> io::Result<u64> {
    let mut src = open_regular_file_at(src_dir, name)?;
    let mut dst = create_file_at(dst_dir, name, mode)?;
    io::copy(&mut src, &mut dst).inspect_err(|_| {
        // A partial copy under the final name would look complete to the next reader.
        if let Err(e) = remove_file_at(dst_dir, name) {
            tracing::error!(
                "could not remove the partial copy of {}: {e}",
                name.to_string_lossy()
            );
        }
    })
}

fn open_regular_file_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<std::fs::File> {
    reject_traversal(name)?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = std::fs::File::from(openat(dir, name, flags, Mode::empty())?);
    llmenv_paths::check_regular_file(&file)?;
    Ok(file)
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

/// The modification time of `name` in `dir`, without following a symlink. `None` for a missing
/// entry, and for an mtime before 1970.
///
/// # Errors
/// `name` is not a single component, or the entry exists but cannot be read.
pub(crate) fn mtime_at(
    dir: BorrowedFd<'_>,
    name: &OsStr,
) -> io::Result<Option<std::time::SystemTime>> {
    reject_traversal(name)?;
    let st = match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // `st_mtime_nsec` is `i64` on macOS and `u64` on Linux. A value that does not fit `i64` is out
    // of range, so it becomes -1, which `stat_time` refuses.
    let nanos = i64::try_from(i128::from(st.st_mtime_nsec)).unwrap_or(-1);
    // An out-of-range time is an error, not `None`, so a caller cannot read it as a missing entry.
    stat_time(st.st_mtime, nanos).map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the mtime of {} is out of range ({} s, {} ns)",
                name.to_string_lossy(),
                st.st_mtime,
                st.st_mtime_nsec
            ),
        )
    })
}

/// The time in a stat result, or `None` for a pre-1970 time or a nanosecond part outside one
/// second. Such a part would carry into the seconds and can overflow. Seconds up to `i64::MAX`
/// with nanoseconds under one second fit in a Unix `SystemTime`.
pub(crate) fn stat_time(secs: i64, nanos: i64) -> Option<std::time::SystemTime> {
    let secs = u64::try_from(secs).ok()?;
    let nanos = u32::try_from(nanos).ok().filter(|n| *n < 1_000_000_000)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos))
}

/// Whether `name` in `dir` is a regular file, without following a symlink. `false` for a missing
/// entry.
///
/// # Errors
/// `name` is not a single component, or the entry exists but cannot be read.
pub(crate) fn is_regular_file_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<bool> {
    reject_traversal(name)?;
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(rustix::fs::FileType::from_raw_mode(st.st_mode).is_file()),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Whether the entry `name` in `dir` has an executable bit, without following a symlink. An absent
/// entry has none.
///
/// # Errors
/// `name` is not a single component, or the entry exists but cannot be read.
pub(crate) fn is_executable_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<bool> {
    // 0o111: the owner, group, and other execute bits.
    let execute = Mode::from_raw_mode(0o111);
    Ok(stat_at_nofollow(dir, name)?
        .is_some_and(|st| Mode::from_raw_mode(st.st_mode).intersects(execute)))
}

/// Create `name` in `dir` with `mode`, replacing whatever is there — including a
/// symlink, which is unlinked rather than written through. The file is never briefly more
/// permissive than `mode`.
fn create_file_at(dir: BorrowedFd<'_>, name: &OsStr, mode: Mode) -> io::Result<std::fs::File> {
    reject_traversal(name)?;
    // Unlink first, then create exclusively: `O_TRUNC` on an existing symlink
    // would follow it and truncate the target.
    match remove_file_at(dir, name) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(std::fs::File::from(openat(dir, name, flags, mode)?))
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

/// The `lstat` of `name` in `dir`. `None` when there is no entry with that name.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component, plus `fstatat` errors other than
/// `ENOENT`.
pub(crate) fn stat_at_nofollow(
    dir: BorrowedFd<'_>,
    name: &OsStr,
) -> io::Result<Option<rustix::fs::Stat>> {
    reject_traversal(name)?;
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(st)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write `bytes` to `name` in `dir`, replacing whatever is there — including a
/// symlink, which is unlinked rather than written through.
///
/// # Errors
///
/// `InvalidInput` for anything but a single component, plus the `openat`/write
/// errors. Created with `mode` so the file is never briefly more permissive
/// than intended.
fn write_file_at(dir: BorrowedFd<'_>, name: &OsStr, bytes: &[u8], mode: Mode) -> io::Result<()> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod exec_tests;
