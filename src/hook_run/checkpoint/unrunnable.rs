//! Checkpoints that can never run again, because the working directory of the first run is gone
//! (#2912).

use std::path::Path;

use super::{Checkpoint, complete};

/// Why `checkpoint` can never run again, or `None` when it can. A job runs in the working directory
/// of its first run, so a job whose directory is gone fails on every start and is never pruned,
/// because each failed start restores its file (#2912). A directory that cannot be read for another
/// reason is kept, because the cause can be temporary.
pub(super) fn unrunnable_reason(checkpoint: &Checkpoint) -> Option<String> {
    let Some(cwd) = checkpoint.cwd.as_deref() else {
        return Some("it has no recorded working directory".to_string());
    };
    match std::fs::metadata(cwd) {
        Ok(meta) if meta.is_dir() => None,
        Ok(_) => Some(format!("its working directory {cwd} is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(format!("its working directory {cwd} is gone"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Some(format!(
            "a parent of its working directory {cwd} is not a directory"
        )),
        Err(e) => {
            // A read error can be temporary, so the job is kept.
            crate::agent_notice::record_logged(format!(
                "llmenv: cannot check the working directory {cwd} of a background job ({e}), so \
                 the job is kept. Fix: check the permissions of {cwd}, or remove the job's \
                 checkpoint file."
            ));
            None
        }
    }
}

/// Delete a checkpoint that can never run, and tell the agent once which job it was.
pub(super) fn drop_unrunnable(path: &Path, checkpoint: &Checkpoint, reason: &str) {
    let job = path
        .file_name()
        .map_or_else(|| checkpoint.kind.as_str().into(), |n| n.to_string_lossy());
    match complete(path) {
        Ok(()) => crate::agent_notice::record_logged(format!(
            "llmenv: removed the unfinished background job {job}, because {reason}. The job \
             cannot run again. Fix: none is needed. Start the job again in an existing directory \
             if it is still needed."
        )),
        Err(e) => crate::agent_notice::record_logged(format!(
            "llmenv: the background job {job} cannot run again, because {reason}, but its \
             checkpoint could not be removed ({e:#}). Fix: run `llmenv doctor` to check the \
             state directory."
        )),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod notice_tests {
    use super::*;
    use crate::hook_run::checkpoint::JobKind;

    /// A working directory with a NUL byte makes `metadata` fail with an error that is neither
    /// "not found" nor "not a directory". The job is kept, and the agent is told why (#3217).
    #[test]
    fn unreadable_working_directory_is_recorded_for_the_agent() {
        let mut checkpoint = Checkpoint::new(JobKind::CbmIndex, serde_json::json!({}), None);
        checkpoint.cwd = Some("/llmenv-3217-cwd\0/x".to_string());
        assert_eq!(unrunnable_reason(&checkpoint), None);
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("llmenv-3217-cwd"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A checkpoint that cannot be deleted is named for the agent with its fix. A directory at
    /// the checkpoint path makes `remove_file` fail for a real reason.
    #[test]
    fn failed_unrunnable_removal_is_recorded_for_the_agent() {
        let dir = tempfile::tempdir().expect("test");
        let path = dir.path().join("job-3217-stuck.json");
        std::fs::create_dir(&path).expect("test");
        let checkpoint = Checkpoint::new(JobKind::IcmStore, serde_json::json!({}), None);
        drop_unrunnable(&path, &checkpoint, "its working directory is gone");
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("job-3217-stuck"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A removed checkpoint still states a fix, because every notice ends with one.
    #[test]
    fn removed_unrunnable_job_notice_ends_with_a_fix() {
        let dir = tempfile::tempdir().expect("test");
        let path = dir.path().join("job-3217-removed.json");
        std::fs::write(&path, b"{}").expect("test");
        let checkpoint = Checkpoint::new(JobKind::IcmStore, serde_json::json!({}), None);
        drop_unrunnable(&path, &checkpoint, "its working directory is gone");
        assert!(!path.exists());
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("job-3217-removed"), "{notices}");
        assert!(notices.contains("Fix: none is needed"), "{notices}");
    }
}
