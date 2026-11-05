//! The session filters of the task reminders, over sessions that the caller already read, so one
//! read of the session files feeds a whole hook run (#3468).

use std::collections::HashSet;

use super::Session;
use crate::task::Task;

/// The open sessions of `project` that are not hidden from `caller`. `None` hides no session.
pub(in crate::task) fn open_visible(
    sessions: &[Session],
    project: &str,
    caller: Option<&str>,
) -> Vec<Session> {
    sessions
        .iter()
        .filter(|s| s.is_open() && s.project == project && s.visible_to(caller))
        .cloned()
        .collect()
}

/// The id of every session of `project`, open or closed, that is not hidden from `caller`.
/// `None` gives every session of the project.
pub(in crate::task) fn ids_visible(
    sessions: &[Session],
    project: &str,
    caller: Option<&str>,
) -> HashSet<String> {
    sessions
        .iter()
        .filter(|s| s.project == project && s.visible_to(caller))
        .map(|s| s.id.clone())
        .collect()
}

/// Keep the tasks whose session is in `session_ids`. A task with no session is dropped.
pub(in crate::task) fn tasks_in_sessions(
    tasks: &[Task],
    session_ids: &HashSet<String>,
) -> Vec<Task> {
    tasks
        .iter()
        .filter(|t| {
            t.session
                .as_deref()
                .is_some_and(|sid| session_ids.contains(sid))
        })
        .cloned()
        .collect()
}

/// An open session of `project` for a test, owned by `owner` when it is `Some`.
#[cfg(test)]
pub(in crate::task) fn open_session_for_test(
    id: &str,
    project: &str,
    owner: Option<&str>,
) -> Session {
    Session {
        id: id.to_string(),
        name: None,
        project: project.to_string(),
        description: None,
        started_at: "2026-10-02T00:00:00Z".to_string(),
        last_activity: "2026-10-02T00:00:00Z".to_string(),
        finished_at: None,
        abandoned_at: None,
        owner_session: owner.map(str::to_string),
        owner_pid: None,
        resume: crate::task::resume::ResumeContext::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ids_visible, open_visible};
    use crate::task::resume::ResumeContext;
    use crate::task::session::Session;
    use proptest::prelude::*;

    /// One generated session: its project, its owner, and whether it is open, finished, or
    /// abandoned.
    fn session(i: usize, project: u8, owner: u8, end: u8) -> Session {
        let at = Some("2026-10-02T01:00:00Z".to_string());
        Session {
            id: format!("s{i}"),
            name: None,
            project: ["pa", "pb"][usize::from(project % 2)].to_string(),
            description: None,
            started_at: "2026-10-02T00:00:00Z".to_string(),
            last_activity: "2026-10-02T00:00:00Z".to_string(),
            finished_at: (end == 1).then(|| at.clone()).flatten(),
            abandoned_at: (end == 2).then_some(at).flatten(),
            owner_session: [None, Some("c1".to_string()), Some("c2".to_string())]
                [usize::from(owner % 3)]
            .clone(),
            owner_pid: None,
            resume: ResumeContext::default(),
        }
    }

    fn sessions() -> impl Strategy<Value = Vec<Session>> {
        prop::collection::vec((0u8..2, 0u8..3, 0u8..3), 0..8).prop_map(|rows| {
            rows.into_iter()
                .enumerate()
                .map(|(i, (p, o, e))| session(i, p, o, e))
                .collect()
        })
    }

    fn caller() -> impl Strategy<Value = Option<&'static str>> {
        prop_oneof![Just(None), Just(Some("c1")), Just(Some("c3"))]
    }

    proptest! {
        /// The open sessions are exactly those that are open, of the project, and visible.
        #[test]
        fn open_visible_keeps_exactly_the_open_visible_sessions_of_the_project(
            all in sessions(),
            caller in caller(),
        ) {
            let got: Vec<String> = open_visible(&all, "pa", caller).into_iter().map(|s| s.id).collect();
            let want: Vec<String> = all
                .iter()
                .filter(|s| s.is_open() && s.project == "pa" && s.visible_to(caller))
                .map(|s| s.id.clone())
                .collect();
            prop_assert_eq!(got, want);
        }

        /// A caller only hides sessions: its ids are a subset of the ids with no caller, which
        /// are every session of the project.
        #[test]
        fn a_caller_only_narrows_the_project_ids(all in sessions(), caller in caller()) {
            let every = ids_visible(&all, "pa", None);
            let want: std::collections::HashSet<String> =
                all.iter().filter(|s| s.project == "pa").map(|s| s.id.clone()).collect();
            prop_assert_eq!(&every, &want);
            prop_assert!(ids_visible(&all, "pa", caller).is_subset(&every));
        }
    }
}
