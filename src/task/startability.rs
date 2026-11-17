//! One check of whether `llmenv task start` would start a task, for the reminders (#3770).
//!
//! The start refuses a task for four reasons: a loop in its blocker walk, a task of its session
//! that holds the queue (a task in progress is treated as done, because a reminder names the next
//! start), an ancestor that refuses, and a blocker that is not done. The reminders
//! ask the same four questions, so they never name a start that the start refuses.

use std::collections::HashMap;

use super::{Task, TaskState, ancestors, blockers, relation, session};

/// Why starting `task` would be refused, or `None` when it would start. `all` is every task of the
/// store, and `by_slug` indexes it. A task in progress counts as done for the queue, because a
/// reminder names the start that follows the current task.
pub(super) fn refusal(task: &Task, all: &[Task], by_slug: &HashMap<&str, &Task>) -> Option<String> {
    refusal_with(task, all, by_slug, true)
}

/// [`refusal`] for a start that runs now: a task in progress holds the queue.
pub(super) fn refusal_now(
    task: &Task,
    all: &[Task],
    by_slug: &HashMap<&str, &Task>,
) -> Option<String> {
    refusal_with(task, all, by_slug, false)
}

fn refusal_with(
    task: &Task,
    all: &[Task],
    by_slug: &HashMap<&str, &Task>,
    wip_is_done: bool,
) -> Option<String> {
    blockers::blocker_walk_error(task, by_slug)
        .or_else(|| queue_refusal(task, all, wip_is_done))
        .or_else(|| ancestors::ancestor_refusal(task, all))
        .or_else(|| blockers::unmet_blocker(task, by_slug))
}

/// The refusal of the queue of the session of `task`. With `wip_is_done`, the tasks in progress
/// count as done. A task with no session has no queue.
fn queue_refusal(task: &Task, all: &[Task], wip_is_done: bool) -> Option<String> {
    let id = task.session.as_ref()?;
    let mut session_tasks =
        session::filter::tasks_in_sessions(all, &std::iter::once(id.to_owned()).collect());
    if wip_is_done {
        for t in &mut session_tasks {
            if t.state == TaskState::Wip {
                t.state = TaskState::Done;
            }
        }
    }
    relation::queue_block(task, &session_tasks)
}

/// The first of `open` that would start, in the order given.
pub(super) fn first_startable<'a>(
    open: &[&'a Task],
    all: &[Task],
    by_slug: &HashMap<&str, &Task>,
) -> Option<&'a Task> {
    open.iter()
        .copied()
        .find(|t| refusal(t, all, by_slug).is_none())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;
    use crate::task::{ParentSpec, Relation, add_task_for_session, save_task, start_task};
    use tempfile::TempDir;

    fn mk(slug: &str, state: TaskState, relation: Relation, parent: Option<&str>) -> Task {
        Task {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            state,
            parent: parent.map(str::to_owned),
            relation,
            parallel: false,
            blocked_on: Vec::new(),
            notes: Vec::new(),
            detail: None,
            session: Some("s".to_owned()),
            created_at: format!("2026-10-02T00:00:00.{:09}Z", slug.len()),
            updated_at: "2026-10-02T00:00:00Z".to_owned(),
        }
    }

    fn refusal_of(slug: &str, tasks: &[Task]) -> Option<String> {
        let by_slug: HashMap<&str, &Task> = tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
        let task = tasks.iter().find(|t| t.slug == slug).unwrap();
        refusal(task, tasks, &by_slug)
    }

    #[test]
    fn the_queue_refuses_a_task_behind_an_open_one_and_not_behind_one_in_progress() {
        let mut first = mk("q1", TaskState::Open, Relation::Queued, None);
        let mut second = mk("q2", TaskState::Open, Relation::Queued, None);
        first.created_at = "2026-10-02T00:00:01Z".to_owned();
        second.created_at = "2026-10-02T00:00:02Z".to_owned();
        let why = refusal_of("q2", &[first.clone(), second.clone()]).expect("refused");
        assert!(why.contains("queued behind 'q1'"), "{why}");
        first.state = TaskState::Wip;
        assert_eq!(refusal_of("q2", &[first, second]), None);
    }

    #[test]
    fn a_sub_task_is_refused_for_a_done_missing_or_foreign_parent() {
        let child = mk("c", TaskState::Open, Relation::Child, Some("p"));
        let done = mk("p", TaskState::Done, Relation::Queued, None);
        let why = refusal_of("c", &[child.clone(), done]).expect("refused");
        assert!(why.contains("is done"), "{why}");
        let why = refusal_of("c", std::slice::from_ref(&child)).expect("refused");
        assert!(why.contains("missing"), "{why}");
        let mut foreign = mk("p", TaskState::Open, Relation::Queued, None);
        foreign.session = Some("other".to_owned());
        let why = refusal_of("c", &[child, foreign]).expect("refused");
        assert!(why.contains("belongs to session 'other'"), "{why}");
    }

    #[test]
    fn a_sub_task_with_an_unmet_blocker_is_refused_and_a_free_one_starts() {
        let parent = mk("p", TaskState::Wip, Relation::Queued, None);
        let mut blocked = mk("c1", TaskState::Open, Relation::Child, Some("p"));
        blocked.blocked_on = vec!["other".to_owned()];
        let free = mk("c2", TaskState::Open, Relation::Child, Some("p"));
        let other = mk("other", TaskState::Wip, Relation::Queued, None);
        let tasks = [parent, blocked, free, other];
        let why = refusal_of("c1", &tasks).expect("refused");
        assert!(why.contains("waits on 'other'"), "{why}");
        assert_eq!(refusal_of("c2", &tasks), None);
    }

    // The reminder check and the real start agree: a task that `refusal` accepts starts, and one
    // that it refuses is refused by `start_task` too (#3770).
    #[test]
    fn the_check_agrees_with_the_start() {
        let dir = TempDir::new().unwrap();
        let first = add_task_for_session(dir.path(), "First", ParentSpec::Detached, "s").unwrap();
        let second = add_task_for_session(dir.path(), "Second", ParentSpec::Detached, "s").unwrap();
        let all = crate::task::list_tasks(dir.path());
        let by_slug: HashMap<&str, &Task> = all.iter().map(|t| (t.slug.as_str(), t)).collect();
        let refused = refusal(
            all.iter().find(|t| t.slug == second.slug).unwrap(),
            &all,
            &by_slug,
        );
        assert!(refused.is_some());
        assert!(start_task(dir.path(), &second.slug, false).is_err());
        let accepted = refusal(
            all.iter().find(|t| t.slug == first.slug).unwrap(),
            &all,
            &by_slug,
        );
        assert_eq!(accepted, None);
        assert!(start_task(dir.path(), &first.slug, false).is_ok());
        // A parent that is done refuses the sub-task in both.
        let mut child =
            add_task_for_session(dir.path(), "Child", ParentSpec::Detached, "s").unwrap();
        child.parent = Some(first.slug.clone());
        child.relation = Relation::Child;
        save_task(dir.path(), &child).unwrap();
        crate::task::complete_task(dir.path(), &first.slug, true).unwrap();
        let all = crate::task::list_tasks(dir.path());
        let by_slug: HashMap<&str, &Task> = all.iter().map(|t| (t.slug.as_str(), t)).collect();
        let child = all.iter().find(|t| t.slug == child.slug).unwrap();
        assert!(refusal(child, &all, &by_slug).is_some());
        assert!(start_task(dir.path(), &child.slug, false).is_err());
    }

    // The stalled-parent reminder names the first sub-task that the start accepts, and when none
    // is accepted it names the first one with its reason (#3770).
    #[test]
    fn the_stalled_parent_reminder_skips_a_blocked_sub_task() {
        let parent = mk("p", TaskState::Wip, Relation::Queued, None);
        let mut blocked = mk("c1", TaskState::Open, Relation::Child, Some("p"));
        blocked.blocked_on = vec!["other".to_owned()];
        blocked.created_at = "2026-10-02T00:00:01Z".to_owned();
        let mut free = mk("c2", TaskState::Open, Relation::Child, Some("p"));
        free.created_at = "2026-10-02T00:00:02Z".to_owned();
        let other = mk("other", TaskState::Wip, Relation::Queued, None);
        let ids = ["s".to_owned()];
        let all = vec![parent.clone(), blocked.clone(), free, other.clone()];
        let lines = relation::stalled_parent_lines(&all, &ids);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("llmenv task start c2"), "{}", lines[0]);
        let all = vec![parent, blocked, other];
        let lines = relation::stalled_parent_lines(&all, &ids);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("'c1' cannot start"), "{}", lines[0]);
        assert!(!lines[0].contains("llmenv task start c1"), "{}", lines[0]);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        // For small random stores of open and done tasks, the reminder check accepts a task exactly
        // when `start_task` accepts it. Each task is tried in a fresh store, because a start
        // changes the store. A task in progress is left out: the check treats it as done on
        // purpose, because a reminder names the start that follows it (#3770).
        #[test]
        fn the_check_accepts_exactly_what_the_start_accepts(
            specs in proptest::collection::vec(
                (proptest::prelude::any::<bool>(), 0u8..3, proptest::option::of(0usize..5)),
                1..5,
            ),
        ) {
            let tasks: Vec<Task> = specs
                .iter()
                .enumerate()
                .map(|(i, (open, kind, blocker))| {
                    let state = if *open { TaskState::Open } else { TaskState::Done };
                    let slug = format!("t{i}");
                    let mut t = match (*kind, i) {
                        (2, 1..) => mk(&slug, state, Relation::Child, Some(&format!("t{}", i - 1))),
                        _ => mk(&slug, state, Relation::Queued, None),
                    };
                    t.parallel = *kind == 1;
                    t.created_at = format!("2026-10-02T00:00:{i:02}Z");
                    if let Some(b) = blocker.filter(|b| *b < specs.len() && *b != i) {
                        t.blocked_on = vec![format!("t{b}")];
                    }
                    t
                })
                .collect();
            let by_slug: HashMap<&str, &Task> =
                tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
            for task in tasks.iter().filter(|t| t.state == TaskState::Open) {
                let dir = TempDir::new().unwrap();
                for t in &tasks {
                    save_task(dir.path(), t).unwrap();
                }
                let accepted = refusal(task, &tasks, &by_slug).is_none();
                let started = start_task(dir.path(), &task.slug, false).is_ok();
                proptest::prop_assert_eq!(
                    accepted,
                    started,
                    "task {} refusal {:?} in {:?}",
                    task.slug,
                    refusal(task, &tasks, &by_slug),
                    tasks.iter().map(|t| (&t.slug, t.state, &t.parent, t.parallel, &t.blocked_on)).collect::<Vec<_>>()
                );
            }
        }
    }
}
