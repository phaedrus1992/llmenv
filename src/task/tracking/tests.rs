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
                    .min_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.slug.cmp(&b.slug)))
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
