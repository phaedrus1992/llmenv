use super::{Tracking, tracking_in, tracking_of};
use crate::task::session::filter::open_session_for_test;
use crate::task::{Stored, StrictTasks};
use crate::task::{Task, TaskState};

proptest::proptest! {
    /// A store that cannot be read is `Unknown` with the cause, for any caller, never a guess
    /// such as `NoSession` or `NoTasks` (#3472).
    #[test]
    fn a_read_error_is_unknown_with_its_cause(
        msg in "[a-z]{1,12}",
        caller in proptest::option::of("[a-z]{1,4}"),
        sessions_fail in proptest::prelude::any::<bool>(),
    ) {
        let stored = if sessions_fail {
            Stored::from_parts(
                Ok(StrictTasks::from_vec_for_test(Vec::new())),
                Err(anyhow::anyhow!("{msg}")),
            )
        } else {
            Stored::from_parts(Err(anyhow::anyhow!("{msg}")), Ok(Vec::new()))
        };
        // A project that does not resolve is also `Unknown`, so either way the check holds.
        match tracking_in(&stored, caller.as_deref()) {
            Tracking::Unknown(text) => {
                let named = text.contains(&msg) || text.contains("the project cannot be named");
                proptest::prop_assert!(named, "{text}");
            }
            other => proptest::prop_assert!(false, "expected Unknown, got {other:?}"),
        }
    }
}

/// A task of session `session` in `state`, created at `at`.
fn task(slug: &str, session: &str, state: TaskState, at: u8) -> Task {
    let mut task: Task = serde_json::from_value(serde_json::json!({
        "slug": slug,
        "title": slug,
        "created_at": format!("2026-01-01T00:00:{at:02}Z"),
        "updated_at": "",
    }))
    .unwrap_or_else(|e| unreachable!("a minimal task parses: {e}"));
    task.session = Some(session.to_string());
    task.state = state;
    task
}

fn state_of(i: u8) -> TaskState {
    match i % 4 {
        0 => TaskState::Open,
        1 => TaskState::Wip,
        2 => TaskState::Waiting,
        _ => TaskState::Done,
    }
}

proptest::proptest! {
    /// The fields of `Tracked` follow the tasks of the visible sessions: `queued` counts their
    /// open tasks, `next` is the earliest open one, `wip` is a task in progress, and `waiting`
    /// lists the waiting ones. A task of a session that is not visible changes nothing.
    #[test]
    fn the_tracked_fields_follow_the_visible_tasks(
        rows in proptest::collection::vec((0u8..3, 0u8..4, 0u8..50), 0..10),
    ) {
        // Sessions s0 and s1 are visible; s2 is not in the list, like a hidden or closed one.
        let visible = [
            open_session_for_test("s0"),
            open_session_for_test("s1"),
        ];
        let tasks: Vec<Task> = rows
            .iter()
            .enumerate()
            .map(|(i, (s, st, at))| task(&format!("t{i}"), &format!("s{s}"), state_of(*st), *at))
            .collect();
        let mine: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.session.as_deref() != Some("s2") && t.state != TaskState::Done)
            .collect();
        let open: Vec<&&Task> = mine.iter().filter(|t| t.state == TaskState::Open).collect();
        match tracking_of(&visible, &tasks, None) {
            Tracking::NoTasks => proptest::prop_assert!(mine.is_empty()),
            Tracking::Tracked { wip, waiting, next, queued, .. } => {
                proptest::prop_assert!(!mine.is_empty());
                proptest::prop_assert_eq!(queued, open.len());
                let earliest = open
                    .iter()
                    .min_by(|a, b| crate::util::time::cmp_instants(&a.created_at, &b.created_at).then_with(|| a.slug.cmp(&b.slug)))
                    .map(|t| t.slug.to_string());
                proptest::prop_assert_eq!(next, earliest);
                let wip_ok = match &wip {
                    Some(slug) => mine.iter().any(|t| &t.slug == slug && t.state == TaskState::Wip),
                    None => !mine.iter().any(|t| t.state == TaskState::Wip),
                };
                proptest::prop_assert!(wip_ok, "{wip:?}");
                let want: Vec<String> = mine
                    .iter()
                    .filter(|t| t.state == TaskState::Waiting)
                    .map(|t| t.slug.to_string())
                    .collect();
                proptest::prop_assert_eq!(waiting, want);
            }
            other => proptest::prop_assert!(false, "unexpected {other:?}"),
        }
        proptest::prop_assert!(matches!(tracking_of(&[], &tasks, None), Tracking::NoSession));
    }
}

/// #3757: the reminder skips an open task whose blocker is not done, and one whose blocker walk
/// loops, so it never advises a start that the start refuses.
#[test]
fn next_skips_a_task_with_an_unmet_or_looping_blocker() {
    let visible = [open_session_for_test("s0")];
    let blocker = task("blocker", "s0", TaskState::Wip, 1);
    let mut blocked = task("blocked", "s0", TaskState::Open, 2);
    blocked.blocked_on = vec!["blocker".to_owned()];
    let free = task("free", "s0", TaskState::Open, 3);
    // Parallel tasks sit outside the queue, so only the blockers decide in this test.
    let tasks: Vec<Task> = [blocker, blocked, free]
        .into_iter()
        .map(|mut t| {
            t.parallel = true;
            t
        })
        .collect();
    let Tracking::Tracked { next, queued, .. } = tracking_of(&visible, &tasks, None) else {
        unreachable!("tasks are tracked");
    };
    assert_eq!(next.as_deref(), Some("free"));
    assert_eq!(queued, 2, "queued still counts every open task");

    // A blocker that waits on a task of its own parent loop is never done, so the task is skipped.
    let mut a = task("a", "s0", TaskState::Open, 1);
    let mut b = task("b", "s0", TaskState::Open, 2);
    a.blocked_on = vec!["b".to_owned()];
    b.blocked_on = vec!["a".to_owned()];
    a.parallel = true;
    b.parallel = true;
    let Tracking::Tracked { next, stuck, .. } = tracking_of(&visible, &[a, b], None) else {
        unreachable!("tasks are tracked");
    };
    assert_eq!(next, None);
    // #3763: the reminder gets the reason, and the reason names the first open task.
    let Some((slug, why)) = stuck else {
        unreachable!("a stuck reason is set")
    };
    assert_eq!(slug, "a");
    assert!(!why.is_empty());
}

proptest::proptest! {
    /// With blocker edges, `next` is the earliest open task whose blockers are all done, and
    /// `queued` still counts every open task. These tasks have no sub-tasks, so a blocker is done
    /// when its own state is `done`. A task that blocks on a missing slug is not actionable (#3757).
    #[test]
    fn next_is_the_earliest_open_task_with_every_blocker_done(
        rows in proptest::collection::vec((0u8..4, proptest::option::of(0usize..12), 0u8..50), 1..10),
    ) {
        let visible = [open_session_for_test("s0")];
        let tasks: Vec<Task> = rows
            .iter()
            .enumerate()
            .map(|(i, (st, blocker, at))| {
                let mut t = task(&format!("t{i}"), "s0", state_of(*st), *at);
                if let Some(b) = blocker {
                    t.blocked_on = vec![format!("t{b}")];
                }
                t.parallel = true;
                t
            })
            .collect();
        let blockers_done = |t: &Task| {
            t.blocked_on.iter().all(|b| {
                tasks.iter().any(|o| &o.slug == b && o.state == TaskState::Done && o.slug != t.slug)
            })
        };
        let open: Vec<&Task> = tasks.iter().filter(|t| t.state == TaskState::Open).collect();
        let want = open
            .iter()
            .filter(|t| blockers_done(t))
            .min_by(|a, b| crate::util::time::cmp_instants(&a.created_at, &b.created_at).then_with(|| a.slug.cmp(&b.slug)))
            .map(|t| t.slug.clone());
        match tracking_of(&visible, &tasks, None) {
            Tracking::NoTasks => proptest::prop_assert!(tasks.iter().all(|t| t.state == TaskState::Done)),
            Tracking::Tracked { next, queued, .. } => {
                proptest::prop_assert_eq!(next, want);
                proptest::prop_assert_eq!(queued, open.len());
            }
            other => proptest::prop_assert!(false, "unexpected {other:?}"),
        }
    }
}

proptest::proptest! {
    /// `stuck` is set only when there is no next task and a task is still open, and it names the
    /// first open task that a blocker stops. With a next task it is `None` (#3763).
    #[test]
    fn stuck_is_set_only_when_no_task_can_start(
        rows in proptest::collection::vec((0u8..4, proptest::option::of(0usize..12), 0u8..50), 1..10),
    ) {
        let visible = [open_session_for_test("s0")];
        let tasks: Vec<Task> = rows
            .iter()
            .enumerate()
            .map(|(i, (st, blocker, at))| {
                let mut t = task(&format!("t{i}"), "s0", state_of(*st), *at);
                if let Some(b) = blocker {
                    t.blocked_on = vec![format!("t{b}")];
                }
                t.parallel = true;
                t
            })
            .collect();
        if let Tracking::Tracked { next, queued, stuck, .. } = tracking_of(&visible, &tasks, None) {
            proptest::prop_assert_eq!(stuck.is_some(), next.is_none() && queued > 0);
            if let Some((slug, why)) = stuck {
                proptest::prop_assert!(!why.is_empty());
                proptest::prop_assert!(tasks.iter().any(|t| t.slug == slug && t.state == TaskState::Open));
            }
        }
    }
}

proptest::proptest! {
    /// With queued tasks, blockers, and parents, `next` is the earliest open task that the shared
    /// startability check accepts, and `stuck` is set exactly when none is accepted (#3770).
    #[test]
    fn next_follows_the_startability_check_with_the_queue_on(
        rows in proptest::collection::vec((0u8..4, proptest::option::of(0usize..12), 0u8..50), 1..10),
    ) {
        let visible = [open_session_for_test("s0")];
        let tasks: Vec<Task> = rows
            .iter()
            .enumerate()
            .map(|(i, (st, blocker, at))| {
                let mut t = task(&format!("t{i}"), "s0", state_of(*st), *at);
                if let Some(b) = blocker {
                    t.blocked_on = vec![format!("t{b}")];
                }
                t
            })
            .collect();
        let by_slug: std::collections::HashMap<&str, &Task> =
            tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
        let mut open: Vec<&Task> = tasks.iter().filter(|t| t.state == TaskState::Open).collect();
        open.sort_by(|a, b| crate::util::time::cmp_instants(&a.created_at, &b.created_at).then_with(|| a.slug.cmp(&b.slug)));
        let want = crate::task::startability::first_startable(&open, &tasks, &by_slug)
            .map(|t| t.slug.clone());
        if let Tracking::Tracked { next, stuck, queued, .. } = tracking_of(&visible, &tasks, None) {
            proptest::prop_assert_eq!(&next, &want);
            proptest::prop_assert_eq!(queued, open.len());
            proptest::prop_assert_eq!(stuck.is_some(), want.is_none() && !open.is_empty());
        }
    }
}
