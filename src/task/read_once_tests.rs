//! Tests that one task command reads the task store once, from the ownership check to the change
//! (#3485, #3482).

use tempfile::TempDir;

use super::ownership::{Caller, ensure_task_is_ours, read_for_check};
use super::*;

const SESSION: &str = "test-session";

fn mk(dir: &Path, title: &str) -> Task {
    add_task_for_session(dir, title, ParentSpec::Detached, SESSION).expect("add task")
}

/// The strict reads that `command` makes. The command gets the slug and the read of the
/// ownership check, as the CLI commands do.
fn reads_of<T>(
    dir: &Path,
    input: &str,
    command: impl FnOnce(TaskTarget<'_>) -> anyhow::Result<T>,
) -> usize {
    let caller = Caller::Session(SESSION.to_string());
    let before = StrictTasks::reads_on_this_thread();
    let tasks = read_for_check(dir, &[input]).expect("read for the check");
    let (slug, _) = ensure_task_is_ours(dir, &tasks, input, &caller, None).expect("check");
    command(TaskTarget::Checked(&slug, &tasks)).expect("command");
    StrictTasks::reads_on_this_thread() - before
}

#[test]
fn start_reads_the_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    assert_eq!(
        reads_of(dir.path(), &a.slug, |t| start_task(dir.path(), t, false)),
        1
    );
}

#[test]
fn done_reads_the_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    let caller = Caller::Session(SESSION.to_string());
    assert_eq!(
        reads_of(dir.path(), &a.slug, |t| complete_task_by(
            dir.path(),
            t,
            true,
            &caller
        )),
        1
    );
}

#[test]
fn block_reads_the_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    let b = mk(dir.path(), "Task B");
    assert_eq!(
        reads_of(dir.path(), &a.slug, |t| block_task(dir.path(), t, &b.slug)),
        1
    );
}

#[test]
fn edit_reads_the_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    let b = mk(dir.path(), "Task B");
    let block_on = [b.slug.clone()];
    let edit = TaskEdit {
        title: Some("Renamed"),
        block_on: &block_on,
        ..TaskEdit::default()
    };
    assert_eq!(
        reads_of(dir.path(), &a.slug, |t| edit_task(dir.path(), t, &edit)),
        1
    );
}

#[test]
fn reopen_reads_the_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    done_task(dir.path(), &a.slug).expect("close");
    let caller = Caller::Session(SESSION.to_string());
    let before = StrictTasks::reads_on_this_thread();
    let tasks = read_for_check(dir.path(), &[&a.slug]).expect("read");
    let checked = ownership::ensure_tasks_are_ours(dir.path(), &tasks, &[&a.slug], &caller, None)
        .expect("check");
    let slugs: Vec<String> = checked.into_iter().map(|(slug, _)| slug).collect();
    reopen::reopen_resolved(dir.path(), &tasks, &slugs).expect("reopen");
    assert_eq!(StrictTasks::reads_on_this_thread() - before, 1);
}

#[test]
fn a_read_that_a_target_does_not_carry_is_made_by_the_command() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    let before = StrictTasks::reads_on_this_thread();
    start_task(dir.path(), TaskTarget::Resolved(&a.slug), false).expect("start");
    assert_eq!(StrictTasks::reads_on_this_thread() - before, 1);
}

#[test]
fn done_reports_the_session_progress_after_the_close() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    let b = mk(dir.path(), "Task B");
    let first = complete_task(dir.path(), &a.slug, true).expect("close a");
    assert_eq!(first.session_progress, Some((1, 2)));
    let second = complete_task(dir.path(), &b.slug, true).expect("close b");
    assert_eq!(second.session_progress, Some((2, 2)));
    let repeat = complete_task(dir.path(), &b.slug, true).expect("close b again");
    assert_eq!(
        repeat.session_progress,
        Some((2, 2)),
        "a repeat done counts the task once"
    );
}

#[test]
fn a_read_error_of_the_close_keeps_its_text() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    std::fs::write(task_path(dir.path(), "broken"), "{not json").expect("write");
    let err = complete_task(dir.path(), &a.slug, true).expect_err("a broken file is an error");
    assert!(format!("{err:#}").contains("broken"), "{err:#}");
}

#[test]
fn a_read_that_a_write_made_stale_is_not_used() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    done_task(dir.path(), &a.slug).expect("close");
    let caller = Caller::Session(SESSION.to_string());
    let before = StrictTasks::reads_on_this_thread();
    let tasks = read_for_check(dir.path(), &[&a.slug]).expect("read");
    let (slug, _) = ensure_task_is_ours(dir.path(), &tasks, &a.slug, &caller, None).expect("check");
    // `start --reopen`: the reopen writes, so the start must not use the read of the check.
    reopen_task(dir.path(), TaskTarget::Checked(&slug, &tasks)).expect("reopen");
    let started = start_task(dir.path(), TaskTarget::Checked(&slug, &tasks), false)
        .expect("start after the reopen");
    assert_eq!(started.state, TaskState::Wip);
    assert_eq!(StrictTasks::reads_on_this_thread() - before, 2);
}

#[test]
fn done_counts_the_closed_task_from_its_saved_state() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A");
    mk(dir.path(), "Task B");
    let tasks = StrictTasks::read(dir.path()).expect("read");
    let mut closed = load_task(dir.path(), &a.slug).expect("load");
    closed.state = TaskState::Done;
    assert_eq!(progress_after_close(&tasks, &closed), Some((1, 2)));
    closed.session = None;
    assert_eq!(progress_after_close(&tasks, &closed), None);
}

fn open_session(dir: &Path) -> String {
    let project = crate::task::project::current_tag().expect("project tag");
    let outcome = session::start_session(dir, None, None, &project, session::StartDecision::New)
        .expect("start session");
    let session::StartOutcome::Created(session) = outcome else {
        panic!("expected a new session");
    };
    session.id
}

#[test]
fn session_finish_reads_the_task_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let id = open_session(dir.path());
    let task = add_task_for_session(dir.path(), "Step", ParentSpec::Detached, &id).expect("add");
    done_task(dir.path(), &task.slug).expect("close");
    let before = StrictTasks::reads_on_this_thread();
    session::finish_session(dir.path(), &id, false).expect("finish");
    assert_eq!(StrictTasks::reads_on_this_thread() - before, 1);
}

#[test]
fn clear_session_reads_the_task_store_once() {
    let dir = TempDir::new().expect("tempdir");
    let id = open_session(dir.path());
    add_task_for_session(dir.path(), "Step A", ParentSpec::Detached, &id).expect("add");
    add_task_for_session(dir.path(), "Step B", ParentSpec::Detached, &id).expect("add");
    let before = StrictTasks::reads_on_this_thread();
    let deleted = session::delete_tasks_in_session(dir.path(), &id).expect("delete");
    assert_eq!(StrictTasks::reads_on_this_thread() - before, 1);
    assert_eq!(deleted.len(), 2);
}

mod writer_ownership {
    use super::*;
    use crate::task::session::{
        EngineIdentity, StartDecision, StartOutcome, StartRequest, session_reads_on_this_thread,
        start_session_as,
    };

    const PROJECT_TAG: &str = "writer-ownership-0000";

    fn conversation(id: &str) -> EngineIdentity {
        EngineIdentity::from_session_id(Some(id))
    }

    /// An open session that conversation `owner` started.
    fn owned_session(dir: &Path, owner: &EngineIdentity) -> session::Session {
        let request = StartRequest {
            name: Some("owned"),
            description: None,
            project: PROJECT_TAG,
            owner,
            resume: &resume::ResumeContext::default(),
        };
        match start_session_as(dir, &request, StartDecision::New).expect("start session") {
            StartOutcome::Created(session) => session,
            other => panic!("expected a new session, got {other:?}"),
        }
    }

    fn add(dir: &Path, choice: SessionChoice<'_>) -> anyhow::Result<Task> {
        add_task(dir, "Step", ParentSpec::Detached, choice, PROJECT_TAG)
    }

    fn task_count(dir: &Path) -> usize {
        crate::task::store_read::try_list_tasks_counted(dir)
            .expect("list tasks")
            .tasks
            .len()
    }

    #[test]
    fn a_session_of_another_conversation_is_refused_for_every_choice() {
        let dir = TempDir::new().expect("tempdir");
        let session = owned_session(dir.path(), &conversation("conv-a"));
        let intruder = conversation("conv-b");
        let choices = [
            (
                "named",
                SessionChoice::Named {
                    id: &session.id,
                    owner: &intruder,
                    other_session: None,
                },
            ),
            (
                "checked",
                SessionChoice::Checked {
                    session: &session,
                    owner: &intruder,
                },
            ),
            // The sole open session of another conversation is not the default.
            ("resolved", SessionChoice::Resolve(&intruder)),
        ];
        for (name, choice) in choices {
            let err = add(dir.path(), choice).expect_err("a foreign session must refuse the task");
            assert!(
                format!("{err:#}").contains("another conversation"),
                "{name}: {err:#}"
            );
            assert_eq!(task_count(dir.path()), 0, "{name}");
        }
    }

    #[test]
    fn a_checked_session_that_vanished_names_the_failed_add() {
        let dir = TempDir::new().expect("tempdir");
        let me = conversation("conv-a");
        let session = owned_session(dir.path(), &me);
        std::fs::remove_file(
            dir.path()
                .join("tasks")
                .join("sessions")
                .join(format!("{}.json", session.id)),
        )
        .expect("remove the session file");
        let err = add(
            dir.path(),
            SessionChoice::Checked {
                session: &session,
                owner: &me,
            },
        )
        .expect_err("a session without a file takes no task");
        assert!(format!("{err:#}").contains("no task was added"), "{err:#}");
        assert_eq!(task_count(dir.path()), 0);
    }

    #[test]
    fn the_named_owner_override_adds_the_task_and_records_its_note() {
        let dir = TempDir::new().expect("tempdir");
        let session = owned_session(dir.path(), &conversation("conv-a"));
        let intruder = conversation("conv-b");
        let task = add(
            dir.path(),
            SessionChoice::Named {
                id: &session.id,
                owner: &intruder,
                other_session: Some(&session.id),
            },
        )
        .expect("the override names the owner");
        assert_eq!(task.session.as_deref(), Some(session.id.as_str()));
        let saved = load_task(dir.path(), &task.slug).expect("load");
        assert!(
            saved
                .notes
                .iter()
                .any(|n| n.text.contains("--other-session")),
            "{:?}",
            saved.notes
        );
    }

    #[test]
    fn a_session_of_the_caller_takes_the_task_for_every_choice() {
        let dir = TempDir::new().expect("tempdir");
        let me = conversation("conv-a");
        let session = owned_session(dir.path(), &me);
        for choice in [
            SessionChoice::Named {
                id: &session.id,
                owner: &me,
                other_session: None,
            },
            SessionChoice::Checked {
                session: &session,
                owner: &me,
            },
            SessionChoice::Resolve(&me),
        ] {
            let task = add(dir.path(), choice).expect("the owner may add");
            assert_eq!(task.session.as_deref(), Some(session.id.as_str()));
        }
    }

    #[test]
    fn a_named_add_reads_the_session_store_once_and_a_checked_add_not_at_all() {
        let dir = TempDir::new().expect("tempdir");
        let me = conversation("conv-a");
        let session = owned_session(dir.path(), &me);
        let before = session_reads_on_this_thread();
        add(
            dir.path(),
            SessionChoice::Named {
                id: &session.id,
                owner: &me,
                other_session: None,
            },
        )
        .expect("named add");
        assert_eq!(session_reads_on_this_thread() - before, 1);
        let before = session_reads_on_this_thread();
        add(
            dir.path(),
            SessionChoice::Checked {
                session: &session,
                owner: &me,
            },
        )
        .expect("checked add");
        assert_eq!(session_reads_on_this_thread() - before, 0);
    }
}
