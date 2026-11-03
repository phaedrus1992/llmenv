//! Agent notices for a background job that does not restart.

use super::*;
use crate::hook_run::checkpoint::tests::cp;

fn notices() -> String {
    crate::agent_notice::current().unwrap_or_default()
}

#[test]
fn a_job_whose_checkpoint_cannot_be_saved_names_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the checkpoint folder should be stops the save.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let path = blocker.join("job.json");
    let started = restart(&path, &cp(JobKind::Consolidation, 0), 1, &mut |_, _| Ok(()));
    assert!(!started);
    assert!(
        notices().contains("its checkpoint cannot be saved"),
        "{}",
        notices()
    );
}

#[test]
fn a_job_whose_spawn_fails_names_the_attempt_and_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("job.json");
    let spawn = &mut |_: &Checkpoint, _: &Path| Err(anyhow::anyhow!("no spawner"));
    let started = restart(&path, &cp(JobKind::Consolidation, 0), 1, spawn);
    assert!(!started);
    assert!(notices().contains("not resumed (attempt"), "{}", notices());
}
