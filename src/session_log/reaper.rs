//! Best-effort reaping of session-log files whose mtime exceeds a configured
//! retention window. Run once at `SessionStart` when `transcript.retention_days`
//! is set.
//!
//! Only the current session-log file (`session-log.jsonl` in the state dir, or
//! whichever path the file sink is configured to use) is considered for expiry,
//! since the JSONL sink is a single append-only file. Empty parent directories
//! are removed after deletion.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::materialize::cache::{AgeSource, file_stamp};

/// Garbage-collect the session-log file at `log_path` when its age is past
/// `retention_days`. The age is the newer of its mtime and its change time.
/// Best-effort: failures are logged at `error!` and dropped — session logging
/// never fails a launch.
///
/// The directory `log_path` lives in is cleaned up (removed if empty) after
/// deletion to prevent accumulation of empty state directories.
///
/// # Effects
/// Deletes the file on disk. No-op when the file does not exist, when its age
/// cannot be read or the removal fails (both logged at `error!` and noticed), or
/// when retention is zero. The config validator rejects `retention_days: 0`, but
/// this function still handles it defensively.
pub(crate) fn reap_session_log(log_path: &Path, retention_days: u64) {
    reap_session_log_with(
        log_path,
        retention_days,
        AgeSource::MtimeOrCtime,
        SystemTime::now(),
    );
}

/// [`reap_session_log`] with the age source and the clock given. Tests use it to judge an old
/// mtime alone, and to test the retention rule at a later time (#3375).
fn reap_session_log_with(log_path: &Path, retention_days: u64, source: AgeSource, now: SystemTime) {
    if retention_days == 0 {
        return;
    }
    let age = match log_age(log_path, source, now) {
        Ok(Some(age)) => age,
        Ok(None) => return,
        Err(e) => {
            // The reaper runs in the SessionStart hook, where stderr is dropped and the default
            // filter hides a warn, so the failure is an error and a notice (#2862).
            let text = format!(
                "llmenv: the session log reaper cannot read the age of {}, so it is not reaped: \
                 {e}. Fix: check the permissions of the state directory.",
                log_path.display()
            );
            crate::agent_notice::record_logged(text);
            return;
        }
    };
    let retention = Duration::from_secs(retention_days.saturating_mul(86_400));
    if age < retention {
        return;
    }
    if let Err(e) = std::fs::remove_file(log_path) {
        // Hook stderr is dropped, so the agent gets a notice as well as the error log.
        tracing::error!("session_log reaper: failed to remove {log_path:?}: {e}");
        crate::agent_notice::record_logged(format!(
            "llmenv: the session log reaper could not remove {} ({e}), so it stays on disk. Fix: \
             check the permissions of that file and its folder.",
            log_path.display()
        ));
        return;
    }
    tracing::info!(
        path = %log_path.display(),
        retention_days,
        "session_log reaper: removed expired file"
    );
    // Best-effort cleanup of the parent directory if it became empty.
    if let Some(parent) = log_path.parent()
        && dir_is_empty(parent)
    {
        // A non-empty parent is the normal case and is kept quietly. Any other failure is logged.
        if let Err(e) = std::fs::remove_dir(parent)
            && e.kind() != std::io::ErrorKind::DirectoryNotEmpty
        {
            crate::agent_notice::record_logged(format!(
                "llmenv: the session log reaper could not remove the empty folder {} ({e}). Fix: \
                 check the permissions of that folder.",
                parent.display()
            ));
        }
    }
}

/// Whether `dir` has no entries. A read error is noticed and counts as not empty, so the
/// folder is kept.
fn dir_is_empty(dir: &Path) -> bool {
    match dir.read_dir() {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the session log reaper cannot read the folder {} ({e}), so it is kept. \
                 Fix: check the permissions of that folder.",
                dir.display()
            ));
            false
        }
    }
}

/// The age of the file at `path`. `None` for a missing file, and for a stamp in the
/// future, which is kept rather than deleted and logged as clock skew.
fn log_age(path: &Path, source: AgeSource, now: SystemTime) -> std::io::Result<Option<Duration>> {
    // `symlink_metadata` reads the link itself, because the reaper removes the link, not its target.
    // The newer of mtime and change time decides, as in the state sweeps (#3275). A restore with an
    // old mtime leaves a new change time, so a restored log is not removed at once.
    let stamp = match path.symlink_metadata() {
        Ok(meta) => file_stamp(&meta, source)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match now.duration_since(stamp) {
        Ok(age) => Ok(Some(age)),
        Err(_) => {
            // The notice repeats on every SessionStart until the clock is fixed. That repeat is the
            // point: the file is kept until the skew clears, and the agent must see why (#3379).
            crate::agent_notice::record_logged(format!(
                "llmenv: the session log {} has a modified time in the future (clock skew), so it is \
                 kept. Fix: correct the system clock.",
                path.display()
            ));
            Ok(None)
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod age_property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The reaper removes a log exactly when its age reaches the retention, for any age.
        /// The clock is whole seconds, so the boundary is exact (#3375).
        #[test]
        fn a_log_is_removed_exactly_when_its_age_reaches_retention(
            age_secs in 0u64..(60 * 86_400),
            retention_days in 1u64..30,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session-log.jsonl");
            std::fs::write(&path, "{}").unwrap();
            let whole = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let written = SystemTime::UNIX_EPOCH + Duration::from_secs(whole);
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(written)).unwrap();

            reap_session_log_with(
                &path,
                retention_days,
                AgeSource::MtimeOnly,
                written + Duration::from_secs(age_secs),
            );

            let expected_removed = age_secs >= retention_days * 86_400;
            prop_assert_eq!(!path.exists(), expected_removed, "age {}s, retention {}d", age_secs, retention_days);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_log_restored_with_an_old_mtime_is_kept_by_its_change_time() {
        // Restoring a file with an old mtime (cp -p, tar -x) sets its change time to now.
        // The change time is the newer stamp, so the log is judged young and kept (#3292).
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("restored.jsonl");
        std::fs::write(&log, "{}").unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        filetime::set_file_mtime(&log, filetime::FileTime::from_system_time(old)).unwrap();

        let age = log_age(&log, AgeSource::MtimeOrCtime, SystemTime::now())
            .unwrap()
            .unwrap();

        assert!(
            age < Duration::from_secs(86_400),
            "restored log judged {age:?} old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn log_age_of_a_new_symlink_is_the_link_age_not_the_target_age() {
        // The reaper removes the link itself, so its age is the link's own mtime (#3076).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("old.jsonl");
        std::fs::write(&target, "x").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(10 * 86_400);
        std::fs::File::open(&target)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let link = dir.path().join("session-log.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let age = log_age(&link, AgeSource::MtimeOnly, SystemTime::now())
            .unwrap()
            .unwrap();

        assert!(
            age < Duration::from_secs(3600),
            "the link reads as {age:?} old"
        );
    }

    #[test]
    fn log_age_is_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.jsonl");
        assert!(
            log_age(&absent, AgeSource::MtimeOnly, SystemTime::now())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn log_age_reports_an_unreadable_path_as_an_error() {
        // A path under a regular file fails with ENOTDIR, not NotFound (#2862).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let err = log_age(
            &file.join("session-log.jsonl"),
            AgeSource::MtimeOnly,
            SystemTime::now(),
        )
        .unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
        // The reaper logs and returns; it must not panic or delete anything.
        reap_session_log(&file.join("session-log.jsonl"), 1);
        assert!(file.exists());
    }

    #[test]
    fn does_not_delete_file_within_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        // File was just created, so mtime is now — retention of 1 day should
        // keep it.
        let mtime_before = path.metadata().unwrap().modified().unwrap();
        reap_session_log(&path, 1);
        assert!(path.exists(), "file within retention must not be deleted");
        let mtime_after = path.metadata().unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "mtime must not change");
    }

    #[test]
    fn deletes_file_exceeding_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();

        // Push the mtime back past retention.
        let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();

        // Setting the mtime also sets the change time to now, so only the mtime rule can age this file.
        reap_session_log_with(&path, 1, AgeSource::MtimeOnly, SystemTime::now());
        assert!(!path.exists(), "file past retention must be deleted");
    }

    // #3375: the age rule reads the clock it is given, so a later clock can age the same file.
    #[test]
    fn the_retention_rule_follows_the_clock_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let written = SystemTime::now();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(written)).unwrap();

        reap_session_log_with(
            &path,
            7,
            AgeSource::MtimeOnly,
            written + Duration::from_secs(6 * 86_400),
        );
        assert!(
            path.exists(),
            "six days old is inside a seven-day retention"
        );

        reap_session_log_with(
            &path,
            7,
            AgeSource::MtimeOnly,
            written + Duration::from_secs(8 * 86_400),
        );
        assert!(
            !path.exists(),
            "eight days old is past a seven-day retention"
        );
    }

    #[test]
    fn noop_when_file_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.jsonl");
        reap_session_log(&path, 1);
        // No panic — the function should handle gracefully.
    }

    #[test]
    fn noop_when_retention_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        reap_session_log(&path, 0);
        assert!(path.exists(), "retention 0 must not delete");
    }

    #[test]
    fn cleans_up_empty_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        reap_session_log_with(&path, 1, AgeSource::MtimeOnly, SystemTime::now());
        assert!(!path.exists(), "file must be deleted");
        assert!(!dir.path().exists(), "empty parent dir must be removed");
    }

    #[test]
    fn noop_when_mtime_is_in_future() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let future = SystemTime::now() + Duration::from_secs(86_400);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(future)).unwrap();
        reap_session_log(&path, 1);
        assert!(path.exists(), "file with future mtime must not be deleted");
    }
}
