//! The lock of one project's index job (#3914).
//!
//! Every `SessionStart` starts an index job for its project. Two sessions of one project start
//! two, and whether `codebase-memory-mcp` serializes two index runs on one project is not known.
//! The job holds the system lock on a file beside its checkpoint while the indexer runs, so a
//! second job of the same project stops at once. The holder writes its process id and its start
//! time into the file, so a second job can tell the agent about a holder that hangs.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

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
        Ok(()) => {
            record_holder(&file, &lock);
            Ok(Some(file))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(anyhow::anyhow!(
            "the index lock {} cannot be taken: {e}",
            lock.display()
        )),
    }
}

/// Writes the process id and the start time into the lock file. A failed write only costs the
/// hung-holder notice, so it is logged and the job goes on.
fn record_holder(mut file: &File, lock: &Path) {
    use std::io::Write as _;
    let written = file
        .set_len(0)
        .and_then(|()| writeln!(file, "{} {}", std::process::id(), now_secs()));
    if let Err(e) = written {
        tracing::warn!(
            "cannot record the holder in the index lock {}: {e}",
            lock.display()
        );
    }
}

/// Logs that a job of the project holds the lock, so this job stops. A holder that has run
/// longer than `deadline_secs` is probably hung, and the agent is told how to end it.
pub(super) fn note_busy(checkpoint_path: &Path, deadline_secs: u64) {
    let lock = checkpoint_path.with_extension("lock");
    let held = std::fs::read_to_string(&lock).unwrap_or_else(|e| {
        tracing::warn!(
            "cannot read the holder of the index lock {}: {e}",
            lock.display()
        );
        String::new()
    });
    match hung_notice(&held, now_secs(), deadline_secs) {
        Some(notice) => crate::agent_notice::record_logged(notice),
        None => tracing::info!(
            lock = %lock.display(),
            "another index job of this project is running, so this job stops"
        ),
    }
}

/// The notice for a holder that started at the time in `held` (`<pid> <seconds>`) and has run for
/// longer than `deadline`. `None` for a holder that is inside the deadline, or a file that holds
/// no holder.
fn hung_notice(held: &str, now: u64, deadline: u64) -> Option<String> {
    let (pid, since) = held.trim().split_once(' ')?;
    let (pid, since): (u32, u64) = (pid.parse().ok()?, since.parse().ok()?);
    let ran = now.checked_sub(since)?;
    (ran > deadline).then(|| {
        format!(
            "llmenv: the code index job of this project has run for {} min, so a new job did not \
             start. Fix: stop process {pid}, and the next session start runs the index again.",
            ran / 60
        )
    })
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
    fn the_holder_writes_its_process_id_and_start_time() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = dir.path().join("cbm-index-abc.json");
        let _held = take(&checkpoint).unwrap().unwrap();
        let text = std::fs::read_to_string(checkpoint.with_extension("lock")).unwrap();
        let (pid, since) = text.trim().split_once(' ').unwrap();
        assert_eq!(pid, std::process::id().to_string());
        assert!(since.parse::<u64>().unwrap() <= now_secs());
    }

    #[test]
    fn only_a_holder_past_the_deadline_is_called_hung() {
        assert_eq!(
            hung_notice("42 1000", 1_900, 1_800),
            None,
            "inside the deadline"
        );
        assert_eq!(
            hung_notice("42 1000", 2_800, 1_800),
            None,
            "exactly at the deadline"
        );
        let text = hung_notice("42 1000", 2_801, 1_800).unwrap();
        assert!(
            text.contains("process 42") && text.contains("30 min"),
            "{text}"
        );
        assert!(text.contains("Fix:"), "{text}");
    }

    #[test]
    fn a_file_with_no_holder_or_a_future_start_gives_no_notice() {
        for held in ["", "garbage", "x 1", "1 y", "42 9999999"] {
            assert_eq!(hung_notice(held, 100, 10), None, "{held:?}");
        }
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
