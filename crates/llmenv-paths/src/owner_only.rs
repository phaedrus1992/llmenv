//! Owner-only folders and files.

use std::path::Path;

/// Create a directory (and any missing parent components) with owner-only
/// permissions (mode 0o700 on Unix) from the moment of creation, and harden
/// it to 0o700 if it already existed at a looser mode. On Windows falls back
/// to `create_dir_all`'s default permissions.
///
/// Use instead of `create_dir_all` followed by a separate `set_permissions`
/// call for any directory that must never be world-readable — the two-call
/// version leaves the directory at the umask default (typically 0o755)
/// between creation and the chmod, a TOCTOU window (#1113), and a caller who
/// skips the follow-up chmod (or an older llmenv version, before this
/// hardening existed) leaves it world-readable indefinitely (#1178).
///
/// # Errors
/// Propagates directory-creation failure, and failure to chmod an
/// already-existing directory (e.g. owned by another user). On Unix, a symlink
/// at `dir` is an error, and the link target keeps its mode.
pub fn create_dir_owner_only(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // DirBuilder's mode only governs directories it creates. If `dir`
        // already existed (an older llmenv version, a caller that used a
        // bare create_dir_all, a permissive umask), its mode is left
        // untouched -- self-heal it so every caller gets an owner-only
        // directory regardless of whether it was just created.
        harden_existing_dir(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Set an existing folder to 0o700 without following a link at `dir` (#3137). The chmod runs on
/// an open handle, which is race-free. Opening needs read permission, so a folder left unreadable
/// (0o300) by an earlier bug falls back to an lstat check and a chmod by path. That fallback is
/// not atomic: a link put in place between the two calls is followed.
#[cfg(unix)]
fn harden_existing_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let owner_only = std::fs::Permissions::from_mode(0o700);
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY;
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits().cast_signed())
        .open(dir);
    match opened {
        Ok(handle) => handle
            .set_permissions(owner_only)
            .map_err(|e| path_error(dir, "chmod", &e)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
                return Err(path_error(dir, "open", &e));
            }
            std::fs::set_permissions(dir, owner_only).map_err(|e| path_error(dir, "chmod", &e))
        }
        Err(e) => Err(path_error(dir, "open", &e)),
    }
}

/// The error for an owner-only folder that cannot be used. A link (`ELOOP`) or a non-folder
/// (`ENOTDIR`) names the fix that matters. Any other error names the path and the cause.
#[cfg(unix)]
fn path_error(dir: &Path, action: &str, cause: &std::io::Error) -> std::io::Error {
    let not_a_folder = cause.raw_os_error().is_some_and(|code| {
        code == rustix::io::Errno::LOOP.raw_os_error()
            || code == rustix::io::Errno::NOTDIR.raw_os_error()
    });
    let fix = if not_a_folder {
        "Fix: make it a real folder, not a link."
    } else {
        "Fix: check the folder's owner and permissions."
    };
    std::io::Error::new(
        cause.kind(),
        format!(
            "cannot {action} {} as an owner-only folder ({cause}). {fix}",
            dir.display()
        ),
    )
}

/// Write `content` to `path` with owner-only permissions (mode 0o600) on Unix.
/// On Windows falls back to default permissions. Creates the file if absent,
/// truncates if present. Use for any file containing user state or
/// credentials (settings, sync state, MCP configs, ICM memory) where
/// world-readable defaults would leak data on shared systems.
///
/// A FIFO or another special file at `path` is an error on Unix, and the open does not block on it.
/// A symlink at `path` is an error on Unix (`O_NOFOLLOW`), so the write never lands on the link's
/// target (#2686). Only the final component is checked. A symlink in a parent directory is still
/// followed, so the parent must already be a directory the user controls.
pub fn write_owner_only(path: &Path, content: &[u8]) -> std::io::Result<()> {
    write_owner_only_unnamed(path, content, false)
        .map_err(|e| std::io::Error::new(e.kind(), format!("cannot write {}: {e}", path.display())))
}

/// Write `content` as [`write_owner_only`] does, but with the owner executable bit (mode 0o700)
/// on Unix, for a script that a skill or a hook runs directly. The mode is set on the open file,
/// so a file that exists at mode 0o600 gets the bit too. Off Unix this is [`write_owner_only`].
///
/// # Errors
/// As [`write_owner_only`], and a failure to set the mode.
pub fn write_owner_only_executable(path: &Path, content: &[u8]) -> std::io::Result<()> {
    write_owner_only_unnamed(path, content, true)
        .map_err(|e| std::io::Error::new(e.kind(), format!("cannot write {}: {e}", path.display())))
}

/// Write `content` as [`write_owner_only`] does, with the executable bit when the file at `source`
/// has one, so a script copied from a bundle or a skill can still run directly.
///
/// # Errors
/// `source` cannot be read for its metadata, or the write fails.
pub fn write_owner_only_like(path: &Path, content: &[u8], source: &Path) -> std::io::Result<()> {
    if is_executable(source)? {
        write_owner_only_executable(path, content)
    } else {
        write_owner_only(path, content)
    }
}

/// Whether the file at `path` has an executable bit, following a symlink. Off Unix a file has no
/// such bit, so the answer is false.
///
/// # Errors
/// The file cannot be read for its metadata.
pub fn is_executable(path: &Path) -> std::io::Result<bool> {
    let meta = std::fs::metadata(path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("cannot read {}: {e}", path.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(false)
    }
}

/// Open `path` for appending as an owner-only log, and create it when it is missing.
///
/// A new file gets mode 0o600, and an existing looser file is tightened to 0o600. A symlink at
/// `path` is an error on Unix (`O_NOFOLLOW`), so a planted link cannot redirect the appends. Use
/// this for a log that can hold paths, request ids, or child output.
///
/// # Errors
/// The file cannot be opened or created, `path` is a symlink, or the mode cannot be set.
pub fn open_append_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(path)?;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }
}

fn write_owner_only_unnamed(path: &Path, content: &[u8], executable: bool) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = if executable { 0o700 } else { 0o600 };
        if crate::write_stats::already_current(path, content, mode) {
            return Ok(());
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                    .bits()
                    .cast_signed(),
            )
            .open(path)?;
        // A FIFO with no reader fails the open with ENXIO; one with a reader fails here (#2942).
        crate::check_regular_file(&file)?;
        // The open mode applies only to a new file; an existing one keeps its old mode. Enforce
        // the owner-only mode, so a looser file is tightened and the next write can be skipped.
        if file.metadata()?.permissions().mode() & 0o777 != mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(content)?;
    }
    #[cfg(not(unix))]
    {
        let _ = executable;
        std::fs::write(path, content)?;
    }
    crate::write_stats::note_write(content.len());
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
