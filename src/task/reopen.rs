//! Undo for `llmenv task done` (#2471): move `done` tasks back to `open`.
//!
//! `reopen_task` is the lenient single-task step behind `task start --reopen`.
//! `reopen_tasks` is the strict bulk command behind `task reopen`.

use std::path::Path;

use anyhow::Context;

use crate::task::{
    Relation, Task, TaskNote, TaskState, load_task, now_rfc3339, resolve_identifier, save_task,
    touch_task_session, with_store_lock,
};

/// Set a `done` task to `open` and append a note that names the command used.
fn mark_reopened(task: &mut Task, command: &str) {
    let now = now_rfc3339();
    task.state = TaskState::Open;
    task.notes.push(TaskNote {
        at: now.clone(),
        text: format!("Reopened (`{command}`) after it was marked done."),
    });
    task.updated_at = now;
}

/// Move a `done` task back to `open`, with a note that records the reopen. A
/// task in any other state is returned unchanged, so `task start --reopen`
/// works on a task that is not done.
///
/// # Errors
/// Errors if `input` does not resolve to a task, or the save fails.
pub(crate) fn reopen_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        if task.state != TaskState::Done {
            return Ok(task);
        }
        mark_reopened(&mut task, "task start --reopen");
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// Move every named `done` task back to `open`. Notes, parent, and
/// `blocked_on` links stay as they were.
///
/// If any named task is not `done`, the call changes no task and the error names each task
/// that is not `done`. The same holds for a sub-task whose parent is `done` and not named,
/// because a `done` parent cannot hold an open sub-task. A failed save is the one case that
/// can leave part of the list reopened, and the error then lists which tasks changed.
/// A repeated identifier counts once.
///
/// # Errors
/// Errors if an identifier does not resolve, a task is not `done`, a sub-task has a `done`
/// parent that is not also named, or a save fails. A save error names the tasks that the call
/// already reopened.
pub(crate) fn reopen_tasks(state_dir: &Path, inputs: &[String]) -> anyhow::Result<Vec<Task>> {
    let tasks = with_store_lock(state_dir, || {
        let mut tasks: Vec<Task> = Vec::new();
        for input in inputs {
            let slug = resolve_identifier(state_dir, input)?;
            if tasks.iter().all(|t| t.slug != slug) {
                tasks.push(load_task(state_dir, &slug)?);
            }
        }
        let refused: Vec<String> = tasks
            .iter()
            .filter(|t| t.state != TaskState::Done)
            .map(|t| format!("'{}' is {:?}", t.slug, t.state))
            .collect();
        if !refused.is_empty() {
            anyhow::bail!(
                "only a done task can be reopened, and no task was changed: {}",
                refused.join(", ")
            );
        }
        refuse_done_parents(state_dir, &tasks)?;
        let mut reopened: Vec<String> = Vec::new();
        for task in &mut tasks {
            mark_reopened(task, "task reopen");
            save_task(state_dir, task).with_context(|| {
                format!(
                    "cannot save task '{}'; already reopened: [{}]",
                    task.slug,
                    reopened.join(", ")
                )
            })?;
            reopened.push(task.slug.clone());
        }
        Ok(tasks)
    })?;
    let mut touched: Vec<&str> = Vec::new();
    for task in &tasks {
        if let Some(id) = task.session.as_deref()
            && !touched.contains(&id)
        {
            touched.push(id);
            touch_task_session(state_dir, task);
        }
    }
    Ok(tasks)
}

/// Refuse a sub-task whose parent is `done` and is not in `tasks`. `complete_task` never lets a
/// parent finish before its sub-tasks, so this call must not rebuild that state.
fn refuse_done_parents(state_dir: &Path, tasks: &[Task]) -> anyhow::Result<()> {
    for task in tasks.iter().filter(|t| t.relation == Relation::Child) {
        let Some(parent_slug) = task.parent.as_deref() else {
            continue;
        };
        if tasks.iter().any(|t| t.slug == parent_slug) {
            continue;
        }
        let parent = load_task(state_dir, parent_slug)?;
        if parent.state == TaskState::Done {
            anyhow::bail!(
                "no task was changed: '{}' is a sub-task of '{parent_slug}', which is done. \
                 Name '{parent_slug}' in the same call to reopen both.",
                task.slug
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::task::{
        NewTask, ParentSpec, Placement, add_task_for_session, add_task_for_session_with,
        complete_task, start_task,
    };
    use tempfile::TempDir;

    fn open(dir: &Path, title: &str) -> Task {
        add_task_for_session(dir, title, ParentSpec::Detached, "test-session").unwrap()
    }

    fn done(dir: &Path, title: &str) -> Task {
        let task = open(dir, title);
        complete_task(dir, &task.slug, true).unwrap();
        task
    }

    #[test]
    fn reopen_tasks_restores_every_done_task_and_keeps_notes() {
        let dir = TempDir::new().unwrap();
        let a = done(dir.path(), "Alpha task");
        let b = done(dir.path(), "Bravo task");
        let notes_before = load_task(dir.path(), &a.slug).unwrap().notes.len();

        let got = reopen_tasks(dir.path(), &[a.slug.clone(), b.slug.clone()]).unwrap();

        assert_eq!(got.len(), 2);
        for slug in [&a.slug, &b.slug] {
            assert_eq!(load_task(dir.path(), slug).unwrap().state, TaskState::Open);
        }
        let reloaded = load_task(dir.path(), &a.slug).unwrap();
        assert_eq!(reloaded.notes.len(), notes_before + 1);
        assert!(reloaded.notes.last().unwrap().text.contains("task reopen"));
    }

    #[test]
    fn reopen_tasks_refuses_a_non_done_task_and_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let closed = done(dir.path(), "Closed task");
        let live = open(dir.path(), "Live task");
        start_task(dir.path(), &live.slug, false).unwrap();

        let err = reopen_tasks(dir.path(), &[closed.slug.clone(), live.slug.clone()])
            .unwrap_err()
            .to_string();

        assert!(err.contains(&live.slug), "{err}");
        assert!(!err.contains(&closed.slug), "{err}");
        assert_eq!(
            load_task(dir.path(), &closed.slug).unwrap().state,
            TaskState::Done,
            "a refused call must leave the done task done"
        );
    }

    #[test]
    fn reopen_tasks_counts_a_repeated_identifier_once() {
        let dir = TempDir::new().unwrap();
        let a = done(dir.path(), "Alpha task");
        let got = reopen_tasks(dir.path(), &[a.slug.clone(), a.slug.clone()]).unwrap();
        assert_eq!(got.len(), 1);
        let notes = load_task(dir.path(), &a.slug).unwrap().notes;
        assert_eq!(
            notes.iter().filter(|n| n.text.contains("Reopened")).count(),
            1
        );
    }

    #[test]
    fn reopen_tasks_errors_on_an_unknown_slug() {
        let dir = TempDir::new().unwrap();
        let err = reopen_tasks(dir.path(), &["no-such-task".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no-such-task"), "{err}");
    }

    #[test]
    fn reopen_tasks_keeps_the_parent_link() {
        let dir = TempDir::new().unwrap();
        let parent = open(dir.path(), "Parent task");
        let child = add_task_for_session(
            dir.path(),
            "Child task",
            ParentSpec::Explicit(&parent.slug),
            "test-session",
        )
        .unwrap();
        assert!(child.parent.is_some());
        complete_task(dir.path(), &child.slug, true).unwrap();
        reopen_tasks(dir.path(), std::slice::from_ref(&child.slug)).unwrap();
        let after = load_task(dir.path(), &child.slug).unwrap();
        assert_eq!(after.parent, child.parent);
    }

    fn sub_task(dir: &Path, title: &str, parent: &str) -> Task {
        let new = NewTask {
            title,
            placement: Placement::Child(parent),
            ..NewTask::default()
        };
        add_task_for_session_with(dir, &new, ParentSpec::Detached, "test-session").unwrap()
    }

    #[test]
    fn reopen_tasks_refuses_a_sub_task_under_a_done_parent() {
        let dir = TempDir::new().unwrap();
        let parent = open(dir.path(), "Parent task");
        let child = sub_task(dir.path(), "Child task", &parent.slug);
        complete_task(dir.path(), &child.slug, true).unwrap();
        complete_task(dir.path(), &parent.slug, true).unwrap();

        let err = reopen_tasks(dir.path(), std::slice::from_ref(&child.slug))
            .unwrap_err()
            .to_string();

        assert!(err.contains(&parent.slug), "{err}");
        assert_eq!(
            load_task(dir.path(), &child.slug).unwrap().state,
            TaskState::Done
        );
    }

    #[test]
    fn reopen_tasks_accepts_a_sub_task_with_its_done_parent() {
        let dir = TempDir::new().unwrap();
        let parent = open(dir.path(), "Parent task");
        let child = sub_task(dir.path(), "Child task", &parent.slug);
        complete_task(dir.path(), &child.slug, true).unwrap();
        complete_task(dir.path(), &parent.slug, true).unwrap();

        reopen_tasks(dir.path(), &[child.slug.clone(), parent.slug.clone()]).unwrap();

        for slug in [&child.slug, &parent.slug] {
            assert_eq!(load_task(dir.path(), slug).unwrap().state, TaskState::Open);
        }
    }

    #[test]
    fn reopen_tasks_accepts_a_sub_task_with_an_open_parent() {
        let dir = TempDir::new().unwrap();
        let parent = open(dir.path(), "Parent task");
        let child = sub_task(dir.path(), "Child task", &parent.slug);
        complete_task(dir.path(), &child.slug, true).unwrap();
        reopen_tasks(dir.path(), std::slice::from_ref(&child.slug)).unwrap();
    }
}
