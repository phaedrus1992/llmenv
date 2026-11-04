//! Narrows the tasks data of the status line to the sessions one conversation may see (#3196).

use super::data::{SessionProgress, SessionTasks, TasksData};
use crate::task::visibility::owner_visible;

/// Narrow `tasks` to what `caller` may see: replace `session` and `current` with the result of
/// [`visible_tasks`]. Leaves `unreadable` and `sessions` as they are.
pub(crate) fn scope_tasks_to_caller(tasks: &mut TasksData, caller: Option<&str>) {
    let (session, current) = visible_tasks(tasks, caller);
    tasks.session = session;
    tasks.current = current;
}

/// The progress and current task that `caller` may see. Sums the visible sessions in
/// `tasks.sessions`. A file with no `sessions` list (an older llmenv, or no open session) falls
/// back to the aggregate fields.
fn visible_tasks(
    tasks: &TasksData,
    caller: Option<&str>,
) -> (Option<SessionProgress>, Option<String>) {
    if tasks.sessions.is_empty() {
        return (tasks.session, tasks.current.clone());
    }
    let kept: Vec<&SessionTasks> = tasks
        .sessions
        .iter()
        .filter(|s| owner_visible(s.owner.as_deref(), caller))
        .collect();
    if kept.is_empty() {
        return (None, None);
    }
    let (done, total) = kept.iter().fold((0, 0), |acc, s| {
        (acc.0 + s.progress.done, acc.1 + s.progress.total)
    });
    let current = kept.iter().find_map(|s| s.current.clone());
    (Some(SessionProgress { done, total }), current)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
    fn a_file_without_sessions_uses_the_aggregate() {
        let mut data = TasksData {
            session: Some(SessionProgress { done: 2, total: 5 }),
            current: Some("old".to_owned()),
            unreadable: false,
            sessions: Vec::new(),
        };
        scope_tasks_to_caller(&mut data, Some("conv-a"));
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
}
