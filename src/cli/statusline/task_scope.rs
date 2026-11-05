//! Narrows the tasks data of the status line to the sessions one conversation may see (#3196).

use crate::materialize::status_data::session_tasks::sum_progress;
use crate::materialize::status_data::{SessionTasks, TasksData};
use crate::task::visibility::owner_visible;

/// Narrow `tasks` to what `caller` may see. `session` sums the visible sessions in
/// `tasks.sessions`, and `current` is the title of the first visible session that has one. With no
/// visible session both are `None`. `unreadable` and `sessions` are left as they are.
///
/// A file with no session list comes from an older writer, and its aggregate counts every
/// conversation. A caller with no id sees every session, so that aggregate is kept for it. A caller
/// with an id must not see other conversations' work, so the aggregate is hidden (#3196).
pub(crate) fn scope_tasks_to_caller(tasks: &mut TasksData, caller: Option<&str>) {
    if tasks.sessions.is_empty() {
        if caller.is_some() {
            tasks.session = None;
            tasks.current = None;
        }
        return;
    }
    let kept: Vec<&SessionTasks> = tasks
        .sessions
        .iter()
        .filter(|s| owner_visible(s.owner.as_deref(), caller))
        .collect();
    tasks.session = (!kept.is_empty()).then(|| sum_progress(kept.iter().map(|s| s.progress)));
    tasks.current = kept.iter().find_map(|s| s.current.clone());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::materialize::status_data::SessionProgress;
    use proptest::prelude::*;

    fn session(owner: Option<&str>, done: u64, total: u64, current: Option<&str>) -> SessionTasks {
        SessionTasks {
            owner: owner.map(str::to_owned),
            progress: SessionProgress { done, total },
            current: current.map(str::to_owned),
        }
    }

    fn tasks(sessions: Vec<SessionTasks>) -> TasksData {
        TasksData {
            session: None,
            current: None,
            unreadable: false,
            sessions,
        }
    }

    fn two_conversations() -> TasksData {
        tasks(vec![
            session(Some("conv-a"), 1, 2, Some("task A")),
            session(Some("conv-b"), 0, 1, Some("task B")),
        ])
    }

    #[test]
    fn two_conversations_each_see_only_their_own_task() {
        let mut a = two_conversations();
        scope_tasks_to_caller(&mut a, Some("conv-a"));
        assert_eq!(a.session, Some(SessionProgress { done: 1, total: 2 }));
        assert_eq!(a.current.as_deref(), Some("task A"));

        let mut b = two_conversations();
        scope_tasks_to_caller(&mut b, Some("conv-b"));
        assert_eq!(b.session, Some(SessionProgress { done: 0, total: 1 }));
        assert_eq!(b.current.as_deref(), Some("task B"));
    }

    #[test]
    fn a_session_with_no_owner_is_shown_to_every_caller() {
        let mut data = tasks(vec![
            session(None, 1, 1, Some("shared")),
            session(Some("conv-b"), 0, 1, Some("task B")),
        ]);
        scope_tasks_to_caller(&mut data, Some("conv-a"));
        assert_eq!(data.session, Some(SessionProgress { done: 1, total: 1 }));
        assert_eq!(data.current.as_deref(), Some("shared"));
    }

    #[test]
    fn no_caller_id_sums_every_session() {
        let mut data = two_conversations();
        scope_tasks_to_caller(&mut data, None);
        assert_eq!(data.session, Some(SessionProgress { done: 1, total: 3 }));
        assert_eq!(data.current.as_deref(), Some("task A"));
    }

    #[test]
    fn stale_aggregate_fields_are_not_shown_when_no_session_is_listed() {
        let mut data = TasksData {
            session: Some(SessionProgress { done: 2, total: 5 }),
            current: Some("other conversation".to_owned()),
            unreadable: false,
            sessions: Vec::new(),
        };
        scope_tasks_to_caller(&mut data, Some("conv-a"));
        assert_eq!(data.session, None);
        assert_eq!(data.current, None);
    }

    #[test]
    fn a_file_without_sessions_keeps_its_aggregate_for_a_caller_with_no_id() {
        let mut data = TasksData {
            session: Some(SessionProgress { done: 2, total: 5 }),
            current: Some("old".to_owned()),
            unreadable: false,
            sessions: Vec::new(),
        };
        scope_tasks_to_caller(&mut data, None);
        assert_eq!(data.session, Some(SessionProgress { done: 2, total: 5 }));
        assert_eq!(data.current.as_deref(), Some("old"));
    }

    #[test]
    fn a_caller_with_no_visible_session_gets_no_count_or_title() {
        let mut data = tasks(vec![session(Some("conv-b"), 0, 1, Some("task B"))]);
        scope_tasks_to_caller(&mut data, Some("conv-a"));
        assert_eq!(data.session, None);
        assert_eq!(data.current, None);
    }

    proptest! {
        /// The count is the sum over the visible sessions, and it is absent exactly when none is
        /// visible.
        #[test]
        fn the_count_is_the_sum_of_the_visible_sessions(
            rows in proptest::collection::vec((0usize..3, 0u64..50, 0u64..50), 0..8),
            caller_idx in 0usize..4,
        ) {
            let owners = [None, Some("a"), Some("b")];
            let callers = [None, Some("a"), Some("b"), Some("c")];
            let caller = callers[caller_idx];
            let sessions: Vec<SessionTasks> = rows
                .iter()
                .map(|(o, d, t)| session(owners[*o], *d, *t, None))
                .collect();
            let visible: Vec<&SessionTasks> = sessions
                .iter()
                .filter(|s| owner_visible(s.owner.as_deref(), caller))
                .collect();
            let mut data = TasksData { session: None, current: None, unreadable: false, sessions: sessions.clone() };
            scope_tasks_to_caller(&mut data, caller);
            match data.session {
                None => prop_assert!(visible.is_empty()),
                Some(p) => {
                    prop_assert_eq!(p.done, visible.iter().map(|s| s.progress.done).sum::<u64>());
                    prop_assert_eq!(p.total, visible.iter().map(|s| s.progress.total).sum::<u64>());
                }
            }
        }
    }
}
