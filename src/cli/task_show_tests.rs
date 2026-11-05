//! `llmenv task show --current` and `--next` read the task store strictly (#3471).

use super::{ShowTarget, run_task_show_current_or_next};
use crate::task::session::{StartDecision, StartOutcome, start_session};

#[test]
fn a_corrupt_task_file_fails_task_show_next() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = crate::task::project::current_tag().expect("project tag");
    let outcome =
        start_session(dir.path(), None, None, &project, StartDecision::New).expect("start");
    let StartOutcome::Created(session) = outcome else {
        unreachable!("a new session is created");
    };
    crate::task::add_task_for_session(
        dir.path(),
        "Step",
        crate::task::ParentSpec::Detached,
        &session.id,
    )
    .expect("add");
    let broken = crate::task::tasks_dir(dir.path()).join("broken.json");
    std::fs::write(&broken, "not json").expect("write");
    let err = run_task_show_current_or_next(dir.path(), ShowTarget::Next).expect_err("strict");
    assert!(format!("{err:#}").contains("broken.json"), "{err:#}");
}
