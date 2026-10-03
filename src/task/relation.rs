//! How tasks relate: sub-tasks and the queue (#2455).
//!
//! A sub-task (`Relation::Child`) belongs to a parent. Sub-tasks run in parallel, and the parent
//! cannot finish before they do. A top-level task (`Relation::Queued`) waits for the task ahead
//! of it in its session, unless it is marked `parallel`. The queue is computed from the stored
//! tasks, so deleting a task never leaves a stale link.
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

/// Every sub-task below `slug`, at any depth, that is not `done`.
pub(super) fn undone_descendants<'a>(slug: &str, all: &'a [Task]) -> Vec<&'a Task> {
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
pub(super) struct ChildProgress {
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
    pub(super) fn stalled(self) -> bool {
        self.open > 0 && self.wip == 0 && self.waiting == 0
    }

    pub(super) fn line(self) -> String {
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
pub(super) fn child_progress(parent: &str, all: &[Task]) -> Option<ChildProgress> {
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
pub(super) fn first_open_child<'a>(parent: &str, all: &'a [Task]) -> Option<&'a Task> {
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
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn task(
        slug: &str,
        state: TaskState,
        relation: Relation,
        parent: Option<&str>,
        n: u32,
    ) -> Task {
        Task {
            slug: slug.to_string(),
            title: format!("Title {slug}"),
            state,
            parent: parent.map(str::to_string),
            relation,
            parallel: false,
            blocked_on: Vec::new(),
            notes: Vec::new(),
            detail: None,
            session: Some("s".to_string()),
            created_at: format!("2026-01-01T00:00:{n:02}Z"),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn queued(slug: &str, state: TaskState, n: u32) -> Task {
        task(slug, state, Relation::Queued, None, n)
    }

    #[test]
    fn a_task_stored_without_the_new_fields_loads_as_queued() {
        let json = r#"{"slug":"a","title":"A","state":"open","parent":null,"blocked_on":[],
            "notes":[],"session":"s","created_at":"x","updated_at":"y"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert_eq!(task.relation, Relation::Queued);
        assert!(!task.parallel);
    }

    #[test]
    fn a_queued_task_does_not_write_the_new_fields_and_a_child_does() {
        let plain = serde_json::to_string(&queued("a", TaskState::Open, 1)).unwrap();
        assert!(
            !plain.contains("relation") && !plain.contains("parallel"),
            "{plain}"
        );
        let child =
            serde_json::to_string(&task("c", TaskState::Open, Relation::Child, Some("a"), 2))
                .unwrap();
        assert!(child.contains(r#""relation":"child""#), "{child}");
    }

    #[test]
    fn the_head_of_the_queue_may_start_and_the_next_task_waits() {
        let tasks = [
            queued("a", TaskState::Open, 1),
            queued("b", TaskState::Open, 2),
        ];
        assert_eq!(queue_block(&tasks[0], &tasks), None);
        let block = queue_block(&tasks[1], &tasks).unwrap();
        assert!(block.contains("queued behind 'a' (open)"), "{block}");
    }

    #[test]
    fn a_done_or_waiting_predecessor_releases_the_next_task() {
        for state in [TaskState::Done, TaskState::Waiting] {
            let tasks = [queued("a", state, 1), queued("b", TaskState::Open, 2)];
            assert_eq!(queue_block(&tasks[1], &tasks), None, "{state:?}");
        }
    }

    #[test]
    fn a_task_in_progress_holds_every_other_queued_task() {
        let tasks = [
            queued("a", TaskState::Done, 1),
            queued("b", TaskState::Wip, 2),
            queued("c", TaskState::Open, 3),
            queued("d", TaskState::Open, 4),
        ];
        assert!(
            queue_block(&tasks[2], &tasks)
                .unwrap()
                .contains("'b' (in progress)")
        );
        assert!(
            queue_block(&tasks[3], &tasks)
                .unwrap()
                .contains("'b' (in progress)")
        );
    }

    #[test]
    fn only_an_open_queued_task_is_held() {
        let mut tasks = vec![
            queued("a", TaskState::Open, 1),
            queued("b", TaskState::Waiting, 2),
        ];
        assert_eq!(
            queue_block(&tasks[1], &tasks),
            None,
            "a waiting task resumes"
        );
        tasks.push(task("c", TaskState::Open, Relation::Child, Some("a"), 3));
        assert_eq!(
            queue_block(&tasks[2], &tasks),
            None,
            "a sub-task is not queued"
        );
        let mut beside = queued("d", TaskState::Open, 4);
        beside.parallel = true;
        tasks.push(beside);
        assert_eq!(
            queue_block(&tasks[3], &tasks),
            None,
            "a parallel task has no predecessor"
        );
        let wip_beside = {
            let mut t = queued("e", TaskState::Wip, 5);
            t.parallel = true;
            t
        };
        tasks.push(wip_beside);
        assert_eq!(
            queue_block(&tasks[0], &tasks),
            None,
            "a parallel task is not in the queue"
        );
    }

    #[test]
    fn ties_in_creation_time_break_on_the_slug() {
        let tasks = [
            queued("b", TaskState::Open, 1),
            queued("a", TaskState::Open, 1),
        ];
        assert_eq!(queue_block(&tasks[1], &tasks), None);
        assert!(
            queue_block(&tasks[0], &tasks)
                .unwrap()
                .contains("behind 'a'")
        );
    }

    fn family() -> Vec<Task> {
        vec![
            queued("p", TaskState::Wip, 1),
            task("c1", TaskState::Done, Relation::Child, Some("p"), 2),
            task("c2", TaskState::Wip, Relation::Child, Some("p"), 3),
            task("g1", TaskState::Open, Relation::Child, Some("c2"), 4),
            // A legacy display link is not a sub-task.
            task("l1", TaskState::Open, Relation::Queued, Some("p"), 5),
        ]
    }

    #[test]
    fn undone_descendants_walk_every_depth_and_skip_done_and_legacy_links() {
        let all = family();
        let slugs: Vec<&str> = undone_descendants("p", &all)
            .iter()
            .map(|t| t.slug.as_str())
            .collect();
        assert_eq!(slugs, ["c2", "g1"]);
        assert!(undone_descendants("g1", &all).is_empty());
    }

    #[test]
    fn the_refusal_text_lists_each_open_sub_task() {
        let all = family();
        let text = undone_children_message("p", &undone_descendants("p", &all));
        assert_eq!(
            text,
            "'p' has sub-tasks that are not done: 'c2' (wip), 'g1' (open). Finish them first, or \
             pass --force."
        );
    }

    #[test]
    fn a_cycle_in_the_parent_links_ends() {
        let all = [
            task("a", TaskState::Open, Relation::Child, Some("b"), 1),
            task("b", TaskState::Open, Relation::Child, Some("a"), 2),
        ];
        assert_eq!(undone_descendants("a", &all).len(), 1);
    }

    #[test]
    fn progress_counts_the_direct_children_only() {
        let all = family();
        let p = child_progress("p", &all).unwrap();
        assert_eq!((p.done, p.wip, p.waiting, p.open), (1, 1, 0, 0));
        assert_eq!(
            p.line(),
            "1 of 2 sub-tasks done, 1 in progress, 0 waiting, 0 not started"
        );
        assert_eq!(child_progress("g1", &all), None);
        assert_eq!(progress_suffix("g1", &all), "");
        assert_eq!(
            progress_suffix("p", &all),
            " — 1 of 2 sub-tasks done, 1 in progress, 0 waiting, 0 not started"
        );
    }

    #[test]
    fn a_parent_whose_open_children_all_wait_is_reported_as_waiting() {
        let all = [
            queued("p", TaskState::Wip, 1),
            task("c1", TaskState::Waiting, Relation::Child, Some("p"), 2),
            task("c2", TaskState::Done, Relation::Child, Some("p"), 3),
        ];
        assert!(child_progress("p", &all).unwrap().all_waiting());
        assert_eq!(
            progress_suffix("p", &all),
            " — every open sub-task is waiting on external input"
        );
    }

    #[test]
    fn a_parent_with_only_unstarted_or_done_children_is_stalled() {
        let all = [
            queued("p", TaskState::Wip, 1),
            task("c1", TaskState::Done, Relation::Child, Some("p"), 2),
            task("c2", TaskState::Open, Relation::Child, Some("p"), 3),
        ];
        let lines = stalled_parent_lines(&all, &["s".to_string()]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("1 of 2 sub-tasks done"), "{lines:?}");
        assert!(lines[0].contains("llmenv task start c2"), "{lines:?}");
        assert!(stalled_parent_lines(&all, &["other".to_string()]).is_empty());
    }

    #[test]
    fn a_parent_with_a_running_child_is_not_stalled() {
        // `p` has a running child. Its running child `c2` has an unstarted child of its own.
        let lines = stalled_parent_lines(&family(), &["s".to_string()]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Task 'c2'"), "{lines:?}");
    }

    fn arb_state() -> impl Strategy<Value = TaskState> {
        prop_oneof![
            Just(TaskState::Open),
            Just(TaskState::Wip),
            Just(TaskState::Waiting),
            Just(TaskState::Done)
        ]
    }

    proptest! {
        #[test]
        fn the_first_open_task_is_held_only_by_a_task_in_progress(
            states in prop::collection::vec(arb_state(), 1..8),
        ) {
            let tasks: Vec<Task> = states
                .iter()
                .enumerate()
                .map(|(i, s)| queued(&format!("t{i:02}"), *s, u32::try_from(i).unwrap()))
                .collect();
            let first_open = tasks.iter().position(|t| t.state == TaskState::Open);
            if let Some(i) = first_open {
                let any_wip = tasks.iter().any(|t| t.state == TaskState::Wip);
                let held = queue_block(&tasks[i], &tasks).is_some();
                let predecessor_blocks = i > 0 && !matches!(tasks[i - 1].state, TaskState::Done | TaskState::Waiting);
                prop_assert_eq!(held, any_wip || predecessor_blocks);
            }
        }

        #[test]
        fn undone_descendants_never_hold_a_done_task_or_the_root(
            states in prop::collection::vec(arb_state(), 1..8),
        ) {
            let mut all = vec![queued("root", TaskState::Wip, 0)];
            for (i, s) in states.iter().enumerate() {
                let parent = if i == 0 { "root".to_string() } else { format!("c{}", i - 1) };
                all.push(task(&format!("c{i}"), *s, Relation::Child, Some(&parent), u32::try_from(i + 1).unwrap()));
            }
            let found = undone_descendants("root", &all);
            prop_assert!(found.iter().all(|t| t.state != TaskState::Done && t.slug != "root"));
        }
    }
}
