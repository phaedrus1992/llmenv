//! How tasks relate: sub-tasks and the queue (#2455).
//!
//! A sub-task (`Relation::Child`) belongs to a parent. Sub-tasks run in parallel, and the parent
//! cannot finish before they do. A top-level task (`Relation::Queued`) waits for the task ahead
//! of it in its session, unless it is marked `parallel`. The queue is computed from the stored
//! tasks, so deleting a task never leaves a stale queue link.
//! Design: docs/design/issue-2438-task-tracking-nudges.md

use serde::{Deserialize, Serialize};

use super::{Task, TaskState};

/// How a task relates to the other tasks of its session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// A top-level task. It joins the queue unless `parallel` is set. Tasks stored before this
    /// field existed load as `Queued`.
    #[default]
    Queued,
    /// A sub-task of its `parent`.
    Child,
}

impl Relation {
    /// For `skip_serializing_if`: the default is not written, so old binaries still read the file.
    pub(super) fn is_queued(&self) -> bool {
        *self == Self::Queued
    }
}

/// Where a new task goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Placement<'a> {
    /// The end of the queue.
    #[default]
    Queue,
    /// Beside the head of the queue: no predecessor.
    Parallel,
    /// A sub-task of this task.
    Child(&'a str),
}

/// The queue of a session: its `Queued`, non-parallel tasks in creation order.
fn queue(session_tasks: &[Task]) -> Vec<&Task> {
    let mut queued: Vec<&Task> = session_tasks
        .iter()
        .filter(|t| t.relation == Relation::Queued && !t.parallel)
        .collect();
    queued.sort_by(|a, b| (&a.created_at, &a.slug).cmp(&(&b.created_at, &b.slug)));
    queued
}

/// Why `task` may not start yet, or `None` when it may (#2455). Only an `open` queued task is
/// held: a `waiting` task resumes, and a sub-task or a parallel task has no place in the queue.
/// A task starts when the task ahead of it is `done` or `waiting` and no other queued task of the
/// session is `wip`.
pub(super) fn queue_block(task: &Task, session_tasks: &[Task]) -> Option<String> {
    if task.state != TaskState::Open || task.relation != Relation::Queued || task.parallel {
        return None;
    }
    let queued = queue(session_tasks);
    if let Some(busy) = queued
        .iter()
        .find(|t| t.slug != task.slug && t.state == TaskState::Wip)
    {
        return Some(blocked_message(task, busy, "in progress"));
    }
    let pos = queued.iter().position(|t| t.slug == task.slug)?;
    let ahead = queued.get(pos.checked_sub(1)?)?;
    (!matches!(ahead.state, TaskState::Done | TaskState::Waiting))
        .then(|| blocked_message(task, ahead, ahead.state.as_str()))
}

fn blocked_message(task: &Task, ahead: &Task, state: &str) -> String {
    format!(
        "task '{slug}' is queued behind '{other}' ({state}). Finish '{other}' with `llmenv task \
         done {other}`, or park it with `llmenv task wait {other} \"<reason>\"`. If '{slug}' \
         can run beside it, pass --force.",
        slug = task.slug,
        other = ahead.slug,
    )
}

/// The direct children of `parent`.
fn children<'a>(parent: &str, all: &'a [Task]) -> Vec<&'a Task> {
    all.iter()
        .filter(|t| t.relation == Relation::Child && t.parent.as_deref() == Some(parent))
        .collect()
}

/// Every sub-task below `slug`, at any depth, that is not `done`. `all` is a strict read, so an
/// unreadable sub-task is never missed (#3471).
pub(super) fn undone_descendants<'a>(slug: &str, all: &'a super::StrictTasks) -> Vec<&'a Task> {
    let all = all.as_slice();
    let mut found: Vec<&Task> = Vec::new();
    let mut seen: Vec<&str> = vec![slug];
    let mut frontier: Vec<&str> = vec![slug];
    while let Some(current) = frontier.pop() {
        for child in children(current, all) {
            if seen.contains(&child.slug.as_str()) {
                continue;
            }
            seen.push(child.slug.as_str());
            frontier.push(child.slug.as_str());
            if child.state != TaskState::Done {
                found.push(child);
            }
        }
    }
    found.sort_by(|a, b| (&a.created_at, &a.slug).cmp(&(&b.created_at, &b.slug)));
    found
}

/// The refusal text for `done <parent>` while sub-tasks remain.
pub(super) fn undone_children_message(slug: &str, undone: &[&Task]) -> String {
    let list = undone
        .iter()
        .map(|t| format!("'{}' ({})", t.slug, t.state.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("'{slug}' has sub-tasks that are not done: {list}. Finish them first, or pass --force.")
}

/// How far the sub-tasks of a parent have come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChildProgress {
    pub done: usize,
    pub wip: usize,
    pub waiting: usize,
    pub open: usize,
}

impl ChildProgress {
    fn total(self) -> usize {
        self.done + self.wip + self.waiting + self.open
    }

    /// Every sub-task that is not done waits on the user.
    fn all_waiting(self) -> bool {
        self.waiting > 0 && self.wip == 0 && self.open == 0
    }

    /// Sub-tasks remain, and none is running or parked.
    fn stalled(self) -> bool {
        self.open > 0 && self.wip == 0 && self.waiting == 0
    }

    fn line(self) -> String {
        format!(
            "{} of {} sub-tasks done, {} in progress, {} waiting, {} not started",
            self.done,
            self.total(),
            self.wip,
            self.waiting,
            self.open
        )
    }
}

/// The progress of the direct sub-tasks of `parent`, or `None` when it has none.
fn child_progress(parent: &str, all: &[Task]) -> Option<ChildProgress> {
    let kids = children(parent, all);
    if kids.is_empty() {
        return None;
    }
    let count = |state| kids.iter().filter(|t| t.state == state).count();
    Some(ChildProgress {
        done: count(TaskState::Done),
        wip: count(TaskState::Wip),
        waiting: count(TaskState::Waiting),
        open: count(TaskState::Open),
    })
}

/// The first `open` sub-task of `parent`, in creation order.
fn first_open_child<'a>(parent: &str, all: &'a [Task]) -> Option<&'a Task> {
    children(parent, all)
        .into_iter()
        .filter(|t| t.state == TaskState::Open)
        .min_by(|a, b| (&a.created_at, &a.slug).cmp(&(&b.created_at, &b.slug)))
}

/// The progress of a `wip` parent for a reminder line, with a leading space, or an empty string
/// when the task has no sub-tasks.
pub(super) fn progress_suffix(slug: &str, all: &[Task]) -> String {
    match child_progress(slug, all) {
        Some(p) if p.all_waiting() => {
            " — every open sub-task is waiting on external input".to_string()
        }
        Some(p) => format!(" — {}", p.line()),
        None => String::new(),
    }
}

/// One line for each `wip` parent in `session_ids` whose sub-tasks are all `open` or `done`:
/// the work is planned and nothing runs.
pub(super) fn stalled_parent_lines(all: &[Task], session_ids: &[String]) -> Vec<String> {
    all.iter()
        .filter(|t| t.state == TaskState::Wip)
        .filter(|t| t.session.as_ref().is_some_and(|s| session_ids.contains(s)))
        .filter_map(|parent| {
            let progress = child_progress(&parent.slug, all).filter(|p| p.stalled())?;
            let next = first_open_child(&parent.slug, all)?;
            Some(format!(
                "Task '{}' ({}) has {}. If you recognize it as your own work, run `llmenv task \
                 start {}` ({}) before you work on that step. If you don't recognize it, it \
                 belongs to a different session — leave it alone.",
                parent.slug,
                parent.title,
                progress.line(),
                next.slug,
                next.title
            ))
        })
        .collect()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
