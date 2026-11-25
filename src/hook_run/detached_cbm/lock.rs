//! The lock of one project's index job (#3914).
//!
//! Every `SessionStart` starts an index job for its project. Two sessions of one project start
//! two, and whether `codebase-memory-mcp` serializes two index runs on one project is not known.
//! The job holds the system lock on a file beside its checkpoint while the indexer runs, so a
//! second job of the same project stops at once.

use std::fs::{File, TryLockError};
use std::path::Path;

/// The lock of the job whose checkpoint is `checkpoint_path`, or `None` when another job holds it.
/// The file stays after the job and holds nothing: the lock is the system lock on the open file,
/// which the system releases when the job exits, even after a crash.
///
/// # Errors
/// The lock file cannot be opened, or the system refuses the lock for a reason other than a
/// holder.
pub(super) fn take(checkpoint_path: &Path) -> anyhow::Result<Option<File>> {
    let lock = checkpoint_path.with_extension("lock");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    crate::paths::refuse_symlink(&mut options);
    let file = options.open(&lock).map_err(|e| {
        anyhow::anyhow!(
            "the index lock {} cannot be opened: {e}. Fix: check that the state folder is writable.",
            lock.display()
        )
    })?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(anyhow::anyhow!(
            "the index lock {} cannot be taken: {e}",
            lock.display()
        )),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_second_job_of_the_project_cannot_take_the_lock_until_the_first_ends() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = dir.path().join("cbm-index-abc.json");
        let first = take(&checkpoint).unwrap();
        assert!(first.is_some());
        assert!(
            take(&checkpoint).unwrap().is_none(),
            "held by the first job"
        );
        drop(first);
        assert!(
            take(&checkpoint).unwrap().is_some(),
            "free after the first job"
        );
    }

    #[test]
    fn two_projects_do_not_share_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let a = take(&dir.path().join("cbm-index-a.json")).unwrap();
        let b = take(&dir.path().join("cbm-index-b.json")).unwrap();
        assert!(a.is_some() && b.is_some());
    }

    #[test]
    fn a_lock_file_that_cannot_be_opened_is_an_error_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = dir.path().join("cbm-index-abc.json");
        std::fs::create_dir(checkpoint.with_extension("lock")).unwrap();
        let err = take(&checkpoint).unwrap_err().to_string();
        assert!(
            err.contains("cannot be opened") && err.contains("Fix:"),
            "{err}"
        );
    }
}
