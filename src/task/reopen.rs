//! Undo for `llmenv task done` (#2471): move `done` tasks back to `open`.
//!
//! `reopen_task` is the lenient single-task step behind `task start --reopen`.
//! `reopen_tasks` is the strict bulk command behind `task reopen`.

use std::path::Path;

use super::TaskTarget;
use super::partial::{PartialWrite, changed_before_failure};
use std::borrow::Cow;

use crate::task::{
    Relation, Task, TaskNote, TaskState, load_task, now_rfc3339, save_task, touch_task_session,
    with_store_lock,
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
pub(crate) fn reopen_task<'a>(
    state_dir: &Path,
    target: impl Into<super::TaskTarget<'a>>,
) -> anyhow::Result<Task> {
    let target = target.into();
    let task = with_store_lock(state_dir, || {
        let slug = target.slug(state_dir)?;
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
#[cfg(test)]
fn reopen_tasks(state_dir: &Path, inputs: &[String]) -> anyhow::Result<Vec<Task>> {
    let targets: Vec<TaskTarget<'_>> = inputs.iter().map(TaskTarget::from).collect();
    reopen_targets(state_dir, &targets)
}

/// [`reopen_tasks`] for the slugs that the ownership check resolved, so the tasks that were
/// checked are the tasks that are reopened, with no second lookup (#3477).
///
/// # Errors
/// The errors of [`reopen_tasks`].
pub(crate) fn reopen_resolved(
    state_dir: &Path,
    checked: &super::StrictTasks,
    slugs: &[String],
) -> anyhow::Result<Vec<Task>> {
    let targets: Vec<TaskTarget<'_>> = slugs
        .iter()
        .map(|s| TaskTarget::Checked(s, checked))
        .collect();
    reopen_targets(state_dir, &targets)
}

fn reopen_targets(state_dir: &Path, targets: &[TaskTarget<'_>]) -> anyhow::Result<Vec<Task>> {
    reopen_saving(state_dir, targets, save_task)
}

/// [`reopen_targets`] with the save as a parameter, so a test can fail one save. A failed save
/// returns a [`PartialWrite`] that holds the tasks already reopened. Their sessions are touched
/// before the error returns.
fn reopen_saving(
    state_dir: &Path,
    targets: &[TaskTarget<'_>],
    mut save: impl FnMut(&Path, &Task) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<Task>> {
    let result = with_store_lock(state_dir, || {
        // One strict read resolves every name (#3477).
        for target in targets {
            target.check()?;
        }
        // The targets of one command share one read: that of the ownership check, or a new one.
        let snapshot = match targets.first() {
            Some(target) => target.tasks(state_dir)?,
            None => Cow::Owned(super::list_tasks_strict(state_dir)?),
        };
        let all_tasks: &super::StrictTasks = &snapshot;
        let mut tasks: Vec<Task> = Vec::new();
        for target in targets {
            let slug = target.slug_in(state_dir, all_tasks)?;
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
        let mut reopened: Vec<Task> = Vec::new();
        for mut task in tasks {
            mark_reopened(&mut task, "task reopen");
            if let Err(e) = save(state_dir, &task) {
                let names: Vec<&str> = reopened.iter().map(|t| t.slug.as_str()).collect();
                let source = e.context(format!(
                    "cannot save task '{}'; already reopened: [{}]. Fix: run `llmenv task reopen` \
                     again for the tasks that are not in that list",
                    task.slug,
                    names.join(", ")
                ));
                return Err(PartialWrite::error(reopened, source));
            }
            reopened.push(task);
        }
        Ok(reopened)
    });
    let saved: &[Task] = match &result {
        Ok(tasks) => tasks,
        Err(e) => changed_before_failure(e),
    };
    let mut touched: Vec<&str> = Vec::new();
    for task in saved {
        if let Some(id) = task.session.as_deref()
            && !touched.contains(&id)
        {
            touched.push(id);
            touch_task_session(state_dir, task);
        }
    }
    result
}

/// Refuse a sub-task whose parent is `done` and is not in `tasks`. `complete_task` never lets a
/// parent finish before its sub-tasks, so this call must not rebuild that state.
fn refuse_done_parents(state_dir: &Path, tasks: &[Task]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    // A parent that is not done stays so for this call, so each one is read once (#3463).
    let mut open_parents = std::collections::HashSet::new();
    for task in tasks.iter().filter(|t| t.relation == Relation::Child) {
        let Some(parent_slug) = task.parent.as_deref() else {
            continue;
        };
        if open_parents.contains(parent_slug) || tasks.iter().any(|t| t.slug == parent_slug) {
            continue;
        }
        let parent = load_task(state_dir, parent_slug).with_context(|| {
            format!(
                "no task was changed: cannot read '{parent_slug}', the parent of '{}'. Fix: \
                 repair or remove {}, then run the command again",
                task.slug,
                super::task_path(state_dir, parent_slug).display()
            )
        })?;
        if parent.state == TaskState::Done {
            anyhow::bail!(
                "no task was changed: '{}' is a sub-task of '{parent_slug}', which is done. \
                 Name '{parent_slug}' in the same call to reopen both.",
                task.slug
            );
        }
        open_parents.insert(parent_slug);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
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

    #[test]
    fn reopen_tasks_accepts_two_sub_tasks_of_one_open_parent() {
        let dir = TempDir::new().unwrap();
        let parent = open(dir.path(), "Parent task");
        let first = sub_task(dir.path(), "First child", &parent.slug);
        let second = sub_task(dir.path(), "Second child", &parent.slug);
        for child in [&first, &second] {
            complete_task(dir.path(), &child.slug, true).unwrap();
        }
        reopen_tasks(dir.path(), &[first.slug.clone(), second.slug.clone()]).unwrap();
        for slug in [&first.slug, &second.slug] {
            assert_eq!(load_task(dir.path(), slug).unwrap().state, TaskState::Open);
        }
    }

    /// The strict read of the store names a corrupt parent file before any task changes
    /// (#3463, #3471).
    #[test]
    fn an_unreadable_parent_file_is_named_and_nothing_changes() {
        let dir = TempDir::new().unwrap();
        let parent = open(dir.path(), "Parent task");
        let child = sub_task(dir.path(), "Child task", &parent.slug);
        complete_task(dir.path(), &child.slug, true).unwrap();
        let path = super::super::task_path(dir.path(), &parent.slug);
        std::fs::write(&path, "not json").unwrap();

        let err = reopen_tasks(dir.path(), std::slice::from_ref(&child.slug)).unwrap_err();

        let text = format!("{err:#}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert_eq!(
            load_task(dir.path(), &child.slug).unwrap().state,
            TaskState::Done
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]

        /// A batch of done sub-tasks is refused exactly when one of them has a done parent that
        /// is not in the batch, however many sub-tasks share a parent (#3463).
        #[test]
        fn a_batch_is_refused_exactly_when_a_done_parent_is_left_out(
            parents in proptest::collection::vec(
                (proptest::prelude::any::<bool>(), proptest::prelude::any::<bool>()),
                1..=3,
            ),
            children in proptest::collection::vec(0usize..3, 1..=4),
        ) {
            let dir = TempDir::new().unwrap();
            let parent_tasks: Vec<Task> = (0..parents.len())
                .map(|i| open(dir.path(), &format!("Parent {i}")))
                .collect();
            let child_tasks: Vec<(usize, Task)> = children
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let index = p % parents.len();
                    let parent = &parent_tasks[index].slug;
                    (index, sub_task(dir.path(), &format!("Child {i}"), parent))
                })
                .collect();
            for (_, child) in &child_tasks {
                complete_task(dir.path(), &child.slug, true).unwrap();
            }
            let mut batch: Vec<String> =
                child_tasks.iter().map(|(_, c)| c.slug.to_string()).collect();
            for (task, (is_done, in_batch)) in parent_tasks.iter().zip(&parents) {
                if *is_done {
                    complete_task(dir.path(), &task.slug, true).unwrap();
                    if *in_batch {
                        batch.push(task.slug.to_string());
                    }
                }
            }
            let refused = child_tasks
                .iter()
                .any(|(index, _)| parents[*index].0 && !parents[*index].1);
            proptest::prop_assert_eq!(reopen_tasks(dir.path(), &batch).is_err(), refused);
        }
    }

    // #3749: a failed second save returns the first reopened task, and the session of that task
    // is still touched.
    #[test]
    fn a_failed_second_reopen_save_returns_the_first_task_and_touches_its_session() {
        use crate::task::session::{StartDecision, StartOutcome, load_existing_session};
        let dir = TempDir::new().unwrap();
        let session = match crate::task::session::start_session(
            dir.path(),
            Some("s"),
            None,
            "reopen-test",
            StartDecision::Auto,
        )
        .unwrap()
        {
            StartOutcome::Created(s) => s,
            other => panic!("expected Created, got {other:?}"),
        };
        let mut slugs = Vec::new();
        for title in ["Alpha task", "Bravo task"] {
            let task =
                add_task_for_session(dir.path(), title, ParentSpec::Detached, &session.id).unwrap();
            complete_task(dir.path(), &task.slug, true).unwrap();
            slugs.push(task.slug);
        }
        let before = load_existing_session(dir.path(), &session.id)
            .unwrap()
            .last_activity;
        let targets: Vec<TaskTarget<'_>> = slugs.iter().map(TaskTarget::from).collect();
        let mut saves = 0;
        let err = reopen_saving(dir.path(), &targets, |state_dir, task| {
            saves += 1;
            if saves == 2 {
                anyhow::bail!("disk full")
            }
            save_task(state_dir, task)
        })
        .unwrap_err();
        let changed = changed_before_failure(&err);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].slug, slugs[0]);
        assert_eq!(
            load_task(dir.path(), &slugs[0]).unwrap().state,
            TaskState::Open
        );
        assert_eq!(
            load_task(dir.path(), &slugs[1]).unwrap().state,
            TaskState::Done
        );
        let after = load_existing_session(dir.path(), &session.id)
            .unwrap()
            .last_activity;
        assert_ne!(
            after, before,
            "the session of the saved task must be touched"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(8))]

        // With a save that fails at the k-th task, the error holds exactly the k tasks before it,
        // those k are open on disk, and the rest stay done (#3749).
        #[test]
        fn a_failed_save_leaves_exactly_the_tasks_before_it_reopened(
            count in 1usize..5,
            fail_at in 0usize..5,
        ) {
            let dir = TempDir::new().unwrap();
            let slugs: Vec<String> =
                (0..count).map(|n| done(dir.path(), &format!("Task number {n}")).slug).collect();
            let targets: Vec<TaskTarget<'_>> = slugs.iter().map(TaskTarget::from).collect();
            let mut saves = 0usize;
            let result = reopen_saving(dir.path(), &targets, |state_dir, task| {
                saves += 1;
                if saves == fail_at + 1 { anyhow::bail!("disk full") }
                save_task(state_dir, task)
            });
            let reopened = fail_at.min(count);
            if fail_at >= count {
                proptest::prop_assert_eq!(result.unwrap().len(), count);
            } else {
                let err = result.unwrap_err();
                proptest::prop_assert_eq!(changed_before_failure(&err).len(), reopened);
                let text = format!("{err:#}");
                proptest::prop_assert!(text.contains("Fix: run"), "{}", text);
            }
            for (n, slug) in slugs.iter().enumerate() {
                let want = if n < reopened { TaskState::Open } else { TaskState::Done };
                proptest::prop_assert_eq!(load_task(dir.path(), slug).unwrap().state, want);
            }
        }
    }
}
