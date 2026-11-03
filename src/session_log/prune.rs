//! Pruning of rotated session-log files by age.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::materialize::cache::{AgeSource, file_stamp, is_expired};
use crate::session_log::rotation::rotation_lock;

/// Remove the rotated files of `path` that were last written more than `older_than` ago. The
/// active file is never removed. Returns the files that were removed (#2816). With `dry_run`, it
/// returns the files that a real run removes, and removes none (#2917).
///
/// A file that cannot be removed does not stop the pass. The error names every file that failed,
/// with the count of files that were removed.
///
/// # Errors
/// Fails when the directory cannot be read, or when any rotated file could not be removed.
pub(crate) fn prune_rotations(
    path: &Path,
    older_than: Duration,
    dry_run: bool,
    age: AgeSource,
) -> anyhow::Result<Vec<PathBuf>> {
    let (Some(dir), Some(base)) = (path.parent(), path.file_name()) else {
        return Ok(Vec::new());
    };
    let Some(entries) = crate::paths::read_dir_optional(dir)? else {
        return Ok(Vec::new());
    };
    let expiry = Expiry {
        now: SystemTime::now(),
        older_than,
        age,
    };
    let mut failures = Vec::new();
    let mut expired = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failures.push(format!("an entry in {}: {e}", dir.display()));
                continue;
            }
        };
        if !is_rotation_of(&entry.file_name(), base) {
            continue;
        }
        match is_old(&entry.path(), expiry) {
            Ok(true) => expired.push(entry.path()),
            Ok(false) => {}
            Err(e) => failures.push(format!("{}: {e}", entry.path().display())),
        }
    }
    let removed = if dry_run || expired.is_empty() {
        expired
    } else {
        remove_under_lock(path, expired, expiry, &mut failures)?
    };
    if !failures.is_empty() {
        anyhow::bail!(
            "{} rotated session-log file(s) could not be checked or removed, {} removed: {}. \
             Fix: check the permissions of the state directory",
            failures.len(),
            removed.len(),
            failures.join("; ")
        );
    }
    Ok(removed)
}

/// The clock, the retention, and the timestamp that judges one rotated file.
#[derive(Clone, Copy)]
pub(super) struct Expiry {
    pub(super) now: SystemTime,
    pub(super) older_than: Duration,
    pub(super) age: AgeSource,
}

/// Whether the file at `path` is past the retention in `expiry`, judged by its age stamp.
fn is_old(path: &Path, expiry: Expiry) -> std::io::Result<bool> {
    let stamp = file_stamp(&std::fs::symlink_metadata(path)?, expiry.age)?;
    Ok(is_expired(expiry.now, stamp, expiry.older_than, path))
}

/// Remove each candidate under the rotation lock, after a second age check.
///
/// A rotation renames each generation up by one. Without the lock, a name judged old can name a
/// newer file by the time it is removed. A file that is gone, or that is no longer old, stays out
/// of the result.
pub(super) fn remove_under_lock(
    path: &Path,
    candidates: Vec<PathBuf>,
    expiry: Expiry,
    failures: &mut Vec<String>,
) -> anyhow::Result<Vec<PathBuf>> {
    let _lock = rotation_lock(path).map_err(|e| {
        anyhow::anyhow!(
            "cannot lock {} to prune its rotations: {e}. Fix: run llmenv clean again when no \
             rotation is in progress",
            path.display()
        )
    })?;
    let mut removed = Vec::new();
    for candidate in candidates {
        match is_old(&candidate, expiry) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                failures.push(format!("{}: {e}", candidate.display()));
                continue;
            }
        }
        match std::fs::remove_file(&candidate) {
            Ok(()) => removed.push(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failures.push(format!("{}: {e}", candidate.display())),
        }
    }
    Ok(removed)
}

/// Whether `name` is `<base>.<digits>`, the name of a rotated file.
pub(super) fn is_rotation_of(name: &std::ffi::OsStr, base: &std::ffi::OsStr) -> bool {
    let (Some(name), Some(base)) = (name.to_str(), base.to_str()) else {
        return false;
    };
    name.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}
