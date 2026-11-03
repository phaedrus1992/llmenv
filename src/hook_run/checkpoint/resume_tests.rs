//! Tests of `resume_pending`.

use super::tests::cp;
use super::*;

#[test]
fn resume_respawns_a_stale_checkpoint_once_with_its_inputs_and_bumps_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let mut stale = cp(JobKind::IcmStore, 1);
    stale.started_at = 1000;
    let path = write(dir.path(), &stale).unwrap().unwrap();
    let now = 1000 + JobKind::IcmStore.deadline_secs() + 1;
    let mut seen = Vec::new();
    let resumed = resume_pending(dir.path(), now, |c, p| {
        seen.push((c.inputs.clone(), c.attempts, p.to_path_buf()));
        Ok(())
    });
    assert_eq!(resumed, 1);
    assert_eq!(seen, vec![(stale.inputs.clone(), 2, path.clone())]);
    let stored = load(&path).unwrap();
    assert_eq!((stored.attempts, stored.started_at), (2, now));
    // The clock restarted, so an immediate second resume leaves it alone.
    assert_eq!(resume_pending(dir.path(), now + 1, |_, _| Ok(())), 0);
}

#[test]
fn resume_skips_a_fresh_checkpoint_and_one_at_the_attempt_cap() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = cp(JobKind::IcmStore, 1);
    write(dir.path(), &fresh).unwrap();
    let mut capped = cp(JobKind::SessionLogRecord, 2);
    capped.started_at = 1;
    capped.attempts = MAX_ATTEMPTS;
    write(dir.path(), &capped).unwrap();
    let far_future = fresh.started_at + 100_000;
    let resumed = resume_pending(dir.path(), far_future, |c, _| {
        assert_eq!(
            c.kind,
            JobKind::IcmStore,
            "only the fresh-turned-stale one runs"
        );
        Ok(())
    });
    assert_eq!(resumed, 1, "the capped checkpoint must not run");
}

#[test]
fn a_start_that_always_fails_stops_at_the_attempt_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut stale = cp(JobKind::IcmStore, 1);
    stale.started_at = 1000;
    let path = write(dir.path(), &stale).unwrap().unwrap();
    let step = JobKind::IcmStore.deadline_secs() + 1;
    let mut starts = 0;
    for n in 1..=5 {
        resume_pending(dir.path(), 1000 + step * n, |_, _| {
            starts += 1;
            anyhow::bail!("spawn failed")
        });
    }
    assert_eq!(
        starts,
        MAX_ATTEMPTS - 1,
        "each failed start uses an attempt"
    );
    assert_eq!(load(&path).unwrap().attempts, MAX_ATTEMPTS);
}

#[test]
fn a_working_directory_under_a_file_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("plain-file");
    std::fs::write(&file, "").unwrap();
    let path = stale_in(
        dir.path(),
        Some(&file.join("project").display().to_string()),
    );
    resume_pending(dir.path(), u64::MAX / 2, |_, _| panic!("must not run"));
    assert!(!path.exists());
}

#[test]
fn at_most_the_cap_of_jobs_start_and_the_rest_wait() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..(MAX_RESUMED_PER_START as u32 + 5) {
        let mut c = cp(JobKind::SessionLogRecord, n);
        c.started_at = 1;
        write(dir.path(), &c).unwrap();
    }
    let now = 1 + JobKind::SessionLogRecord.deadline_secs() + 1;
    let resumed = resume_pending(dir.path(), now, |_, _| Ok(()));
    assert_eq!(resumed, MAX_RESUMED_PER_START);
    let waiting = list(dir.path())
        .iter()
        .filter(|e| matches!(e, Entry::Ready(_, c) if c.attempts == 1))
        .count();
    assert_eq!(
        waiting, 5,
        "the rest keep their attempt count for the next session"
    );
}

#[test]
fn a_kind_that_is_not_resumable_is_never_handed_to_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cp(JobKind::CbmIndex, 1);
    c.started_at = 1;
    let path = write(dir.path(), &c).unwrap().unwrap();
    assert_eq!(
        resume_pending(dir.path(), 1_000_000, |_, _| panic!("spawned")),
        0
    );
    assert_eq!(load(&path).unwrap().attempts, 1);
}

#[test]
fn a_second_resume_in_progress_blocks_this_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut stale = cp(JobKind::IcmStore, 1);
    stale.started_at = 1;
    write(dir.path(), &stale).unwrap();
    let held = lock_state_file(&super::dir(dir.path()), "resume", "test").unwrap();
    assert_eq!(
        resume_pending(dir.path(), 1_000_000, |_, _| panic!(
            "spawned under a held lock"
        )),
        0
    );
    // The agent hears about the skipped resume, not only the log (#3312).
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("the checkpoint resume lock"), "{notices}");
    drop(held);
    assert_eq!(resume_pending(dir.path(), 1_000_000, |_, _| Ok(())), 1);
}

#[test]
fn the_index_job_counts_attempts_across_the_checkpoints_each_session_rewrites() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = serde_json::json!({ "project_root": "/r", "index_path": null });
    let path = begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
    assert_eq!(load(&path).unwrap().attempts, 1);
    begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
    begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
    begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
    assert_eq!(
        load(&path).unwrap().attempts,
        MAX_ATTEMPTS,
        "capped at the limit"
    );
}

/// A stale checkpoint whose first run used `cwd`.
fn stale_in(dir: &Path, cwd: Option<&str>) -> PathBuf {
    let mut stale = cp(JobKind::SessionLogRecord, 7);
    stale.started_at = 1000;
    stale.cwd = cwd.map(str::to_string);
    write(dir, &stale).unwrap().unwrap()
}

#[test]
fn a_checkpoint_whose_working_directory_is_gone_is_removed_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("deleted-project");
    let path = stale_in(dir.path(), Some(&gone.display().to_string()));
    let now = 1000 + JobKind::SessionLogRecord.deadline_secs() + 1;
    let resumed = resume_pending(dir.path(), now, |_, _| {
        panic!("a gone directory must not run")
    });
    assert_eq!(resumed, 0);
    assert!(
        !path.exists(),
        "the checkpoint must be removed, not kept for the next start"
    );
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("session-log-record"), "{notices}");
    assert!(notices.contains("deleted-project"), "{notices}");
}

#[test]
fn a_checkpoint_with_no_recorded_working_directory_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = stale_in(dir.path(), None);
    let resumed = resume_pending(dir.path(), u64::MAX / 2, |_, _| panic!("must not run"));
    assert_eq!(resumed, 0);
    assert!(!path.exists());
}

#[test]
fn a_gone_directory_at_the_attempt_cap_is_removed_too() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    let mut capped = cp(JobKind::IcmStore, 3);
    capped.started_at = 1;
    capped.attempts = MAX_ATTEMPTS;
    capped.cwd = Some(gone.display().to_string());
    let path = write(dir.path(), &capped).unwrap().unwrap();
    resume_pending(dir.path(), u64::MAX / 2, |_, _| Ok(()));
    assert!(!path.exists());
}

#[test]
fn a_fresh_checkpoint_whose_directory_is_gone_is_left_for_its_running_job() {
    let dir = tempfile::tempdir().unwrap();
    let mut fresh = cp(JobKind::IcmStore, 4);
    fresh.cwd = Some(dir.path().join("gone").display().to_string());
    let path = write(dir.path(), &fresh).unwrap().unwrap();
    resume_pending(dir.path(), fresh.started_at, |_, _| Ok(()));
    assert!(path.exists(), "its child may still be running");
}

#[test]
fn a_working_directory_that_is_a_file_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    let path = stale_in(dir.path(), Some(&file.display().to_string()));
    resume_pending(dir.path(), u64::MAX / 2, |_, _| panic!("must not run"));
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn a_working_directory_that_cannot_be_checked_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    let cwd = locked.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let path = stale_in(dir.path(), Some(&cwd.display().to_string()));
    resume_pending(dir.path(), u64::MAX / 2, |_, _| {
        anyhow::bail!("start failed")
    });
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        path.exists(),
        "a cause that can be temporary must not delete the job"
    );
}
