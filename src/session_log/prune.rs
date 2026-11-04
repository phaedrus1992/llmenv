//! Pruning of rotated session-log files by age.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::materialize::cache::{AgeSource, file_stamp, is_expired};
use crate::session_log::rotation::rotation_lock;

/// Remove the rotated files of `path` that are older than `older_than`, judged by `age`. Under
/// `AgeSource::MtimeOrCtime` a rename or a chmod restarts the age, so a rotation that was just
/// renamed or restored stays for one more period (#3321). The
/// active file is never removed. Returns the files that were removed (#2816). With `dry_run`, it
/// returns the files that a real run removes, and removes none (#2917).
///
/// A file that cannot be removed does not stop the pass. The error names every file that failed,
/// with the count of files that were removed, and it carries the files that were removed.
///
/// # Errors
/// Fails when the directory cannot be read, when a rotated file cannot be checked or removed, or
/// when the rotation lock cannot be taken. The error keeps the files removed before the failure.
///
/// Production calls `prune_rotations_at` with its clock. Tests use this form, so it exists only
/// in test builds.
#[cfg(test)]
fn prune_rotations(
    path: &Path,
    older_than: Duration,
    dry_run: bool,
    age: AgeSource,
) -> Result<Vec<PathBuf>, PruneError> {
    prune_rotations_at(path, older_than, dry_run, age, SystemTime::now())
}

/// `prune_rotations`, judged against `now` instead of the clock. A later `now` makes a file that
/// was just written look past retention on its mtime and its change time alike, which a test
/// cannot set on a real file (#3335).
pub(crate) fn prune_rotations_at(
    path: &Path,
    older_than: Duration,
    dry_run: bool,
    age: AgeSource,
    now: SystemTime,
) -> Result<Vec<PathBuf>, PruneError> {
    let (Some(dir), Some(base)) = (path.parent(), path.file_name()) else {
        return Err(PruneError {
            removed: Vec::new(),
            message: format!(
                "cannot prune the rotations of {}: the path has no folder or file name. \
                 Fix: pass the session log file, not a folder.",
                path.display()
            ),
        });
    };
    let entries = crate::paths::read_dir_optional(dir).map_err(|e| PruneError {
        removed: Vec::new(),
        message: format!("{e:#}"),
    })?;
    let Some(entries) = entries else {
        return Ok(Vec::new());
    };
    let expiry = Expiry {
        now,
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
            // A rotation that another pass renames during this scan is not a failure. The
            // locked pass below judges it again.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failures.push(format!("{}: {e}", entry.path().display())),
        }
    }
    let removed = if dry_run || expired.is_empty() {
        expired
    } else {
        match remove_under_lock(path, expired, expiry, &mut failures) {
            Ok(removed) => removed,
            Err(lock_err) => {
                failures.push(format!("{lock_err:#}"));
                Vec::new()
            }
        }
    };
    if !failures.is_empty() {
        // A dry run removes nothing, so its count must not claim removals.
        let verb = if dry_run { "would remove" } else { "removed" };
        let message = format!(
            "{} rotated session-log file(s) could not be checked or removed, {} {verb}: {}. \
             Fix: check the permissions of the listed files and of the state directory",
            failures.len(),
            removed.len(),
            failures.join("; ")
        );
        return Err(PruneError { removed, message });
    }
    Ok(removed)
}

/// A prune pass that failed. It keeps the files it removed before the failure, so a caller can
/// report them next to the error.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct PruneError {
    /// The files that the pass removed, or would remove in a dry run.
    pub(crate) removed: Vec<PathBuf>,
    message: String,
}

/// The clock, the retention, and the timestamp that judges one rotated file.
#[derive(Clone, Copy)]
pub(super) struct Expiry {
    now: SystemTime,
    older_than: Duration,
    age: AgeSource,
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
fn remove_under_lock(
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
///
/// Compares bytes, not `str`: a base that is not UTF-8 would otherwise never match, so its
/// rotations would never be pruned and never reported (#3351).
pub(super) fn is_rotation_of(name: &std::ffi::OsStr, base: &std::ffi::OsStr) -> bool {
    let (name, base) = (name.as_encoded_bytes(), base.as_encoded_bytes());
    name.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix(b"."))
        .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
