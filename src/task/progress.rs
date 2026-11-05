//! Counts and titles over tasks that the caller already read, so one read of the task store
//! feeds a whole hook run or status line render (#3466).

use super::{Task, TaskState, most_recently_updated};

/// The done and total task counts of `session_id` among tasks that the caller already read.
pub(crate) fn progress_in(tasks: &[Task], session_id: &str) -> (u64, u64) {
    let (done, total) = tasks
        .iter()
        .filter(|t| t.session.as_deref() == Some(session_id))
        .fold((0u64, 0u64), |(done, total), t| {
            (done + u64::from(t.state == TaskState::Done), total + 1)
        });
    (done, total)
}

/// The title of the most recently updated `wip` or `waiting` task in `session_ids`, for the
/// statusline's "what's in progress right now" fill-in (#905). `None` when nothing matching is
/// currently `wip`/`waiting`, or when `session_ids` is empty. The caller reads `tasks` strictly,
/// so a corrupt file never removes the title without a sign (#2687).
pub(crate) fn wip_title_in(tasks: &[Task], session_ids: &[String]) -> Option<String> {
    most_recently_updated(tasks.iter().filter(|t| {
        matches!(t.state, TaskState::Wip | TaskState::Waiting)
            && t.session
                .as_deref()
                .is_some_and(|sid| session_ids.iter().any(|s| s == sid))
    }))
    .map(|t| t.title.clone())
}

#[cfg(test)]
mod tests {
    use super::progress_in;
    use crate::task::{Task, TaskState};

    fn task(session: Option<&str>, state: TaskState) -> Task {
        let mut task: Task = serde_json::from_value(serde_json::json!({
            "slug": "t", "title": "t", "created_at": "", "updated_at": "",
        }))
        .unwrap_or_else(|e| unreachable!("a minimal task parses: {e}"));
        task.session = session.map(str::to_owned);
        task.state = state;
        task
    }

    proptest::proptest! {
        /// The counts are the done and total tasks of the session, and no other session's task
        /// changes them.
        #[test]
        fn the_counts_cover_only_the_tasks_of_the_session(
            rows in proptest::collection::vec((0u8..3, proptest::prelude::any::<bool>()), 0..20),
        ) {
            let ids = ["a", "b", "c"];
            let tasks: Vec<Task> = rows
                .iter()
                .map(|(s, done)| {
                    let state = if *done { TaskState::Done } else { TaskState::Open };
                    task(ids.get(usize::from(*s)).copied(), state)
                })
                .collect();
            let mine: Vec<&(u8, bool)> = rows.iter().filter(|(s, _)| *s == 0).collect();
            let done = mine.iter().filter(|(_, d)| *d).count() as u64;
            proptest::prop_assert_eq!(progress_in(&tasks, "a"), (done, mine.len() as u64));
        }
    }
}
