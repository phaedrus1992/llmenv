//! Size rotation of the session-log file, and the prune of its rotated files (#2816).
//!
//! The active file rotates when it reaches [`MAX_BYTES`]. A rotation shifts the older files up
//! one generation, so `<name>.1` holds the newest rotated lines.

use std::path::{Path, PathBuf};

/// Size at which the active file rotates. Each line is one hook event, so the cap bounds how
/// much busy hooks add to one file (#2816).
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Rotated generations kept beside the active file. A rotation drops the oldest one.
pub(crate) const KEPT_ROTATIONS: u32 = 3;

/// Rotate `path` when it holds at least `max_bytes`. Returns the name of the first rotation,
/// or `None` when nothing rotated.
///
/// The rotation holds an advisory lock on a sibling lock file, and checks the length again
/// under it. Two processes can both see a full file. Without the lock, the second would move
/// the first one's new active file into generation 1 (#2831).
pub(crate) fn rotate_if_full(path: &Path, max_bytes: u64) -> std::io::Result<Option<String>> {
    if !is_full(path, max_bytes)? {
        return Ok(None);
    }
    let _lock = rotation_lock(path)?;
    if !is_full(path, max_bytes)? {
        return Ok(None);
    }
    ignore_missing(std::fs::remove_file(rotation_path(path, KEPT_ROTATIONS)))?;
    for n in (1..KEPT_ROTATIONS).rev() {
        ignore_missing(std::fs::rename(
            rotation_path(path, n),
            rotation_path(path, n + 1),
        ))?;
    }
    let first = rotation_path(path, 1);
    std::fs::rename(path, &first)?;
    Ok(first
        .file_name()
        .map(|name| name.to_string_lossy().into_owned()))
}

/// Whether the regular file at path holds at least max_bytes. A missing file is not full.
fn is_full(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(symlink_path_error(path)),
        Ok(meta) if !meta.file_type().is_file() => Err(non_regular_path_error(path)),
        Ok(meta) => Ok(meta.len() >= max_bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!("cannot inspect session log path {}: {e}", path.display()),
        )),
    }
}

fn symlink_path_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "session log path {} is a symbolic link. Fix: set session_log.file.path to a regular file path or remove this link; when unset, the default is <state_dir>/session-log.jsonl",
            path.display()
        ),
    )
}

pub(crate) fn non_regular_path_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "session log path {} is not a regular file. Fix: set session_log.file.path to a regular file path or remove this entry",
            path.display()
        ),
    )
}

/// The suffix of the rotation lock file beside the active log.
const LOCK_SUFFIX: &str = ".rotate.lock";

/// A hook must not stall on a stuck rotation, so the lock gives up after about 500 ms. The
/// append then goes to the active file, and the next append tries the rotation again.
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Take the rotation lock beside `path`. A rotation holds it only for a few renames, so a short
/// bounded wait is enough.
///
/// # Errors
/// The lock file cannot be opened, or the lock stays busy for the whole wait.
pub(super) fn rotation_lock(path: &Path) -> std::io::Result<std::fs::File> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(LOCK_SUFFIX);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = crate::paths::refuse_symlink(&mut opts).open(path.with_file_name(name))?;
    for _ in 0..LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "the session log rotation lock beside {} stays busy",
            path.display()
        ),
    ))
}

/// The first line of a file that follows a rotation. It names the file the old lines moved to.
pub(crate) fn rotation_marker(rotated_to: &str) -> String {
    serde_json::json!({"llmenv_log": "rotated", "rotated_to": rotated_to}).to_string()
}

/// The line that records a failed rotation in the active file. The event follows it there.
pub(crate) fn rotation_failed_marker(error: &std::io::Error) -> String {
    serde_json::json!({"llmenv_log": "rotation_failed", "error": error.to_string()}).to_string()
}

/// The path of rotation `n` of `path`, such as `session-log.jsonl.1`.
pub(crate) fn rotation_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Treat a file that is already gone as done. Any other failure is returned.
fn ignore_missing(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
