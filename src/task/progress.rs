//! Counts and titles over tasks that the caller already read, so one read of the task store
//! feeds a whole hook run or status line render (#3466).

use std::path::Path;

use super::{Task, TaskState, UnreadablePolicy, most_recently_updated, read_tasks};

thread_local! {
    /// How many task files this thread has written or deleted. A read of the store is stale
    /// once this number moves past the one the read saw.
    static WRITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Every task of the store, read so that a task file that cannot be read is an error, not a
/// skipped entry. The field is private and [`StrictTasks::read`] is the one constructor, so a
/// count over this type can never see a partial store (#2608, #2687, #3469).
#[derive(Debug, Clone)]
pub(crate) struct StrictTasks {
    tasks: Vec<Task>,
    /// The write count of the thread when the read ran.
    written_at: u64,
}

impl StrictTasks {
    /// Read every task of the store at `state_dir`.
    ///
    /// # Errors
    /// The task directory or a task file cannot be read or parsed.
    pub(crate) fn read(state_dir: &Path) -> anyhow::Result<Self> {
        let written_at = WRITES.with(std::cell::Cell::get);
        Ok(Self {
            tasks: read_tasks(state_dir, UnreadablePolicy::Fail)?,
            written_at,
        })
    }

    /// Record that this thread wrote or deleted a task file, under the store lock. A read made
    /// before this call is then stale (#3485).
    pub(crate) fn record_write() {
        WRITES.with(|writes| writes.set(writes.get() + 1));
    }

    /// True while this thread has written no task file since the read. A command may use a
    /// current read in place of a new one.
    pub(crate) fn is_current(&self) -> bool {
        self.written_at == WRITES.with(std::cell::Cell::get)
    }

    /// Count one read of the task directory. A test diffs [`Self::reads_on_this_thread`] to
    /// count the reads of one command (#3485).
    #[cfg(test)]
    pub(super) fn count_read() {
        READS.with(|reads| reads.set(reads.get() + 1));
    }

    /// How many reads of the task directory this thread has made.
    #[cfg(test)]
    pub(crate) fn reads_on_this_thread() -> usize {
        READS.with(std::cell::Cell::get)
    }

    /// The tasks, for a caller that only reads them.
    pub(crate) fn as_slice(&self) -> &[Task] {
        &self.tasks
    }

    /// The tasks, for a caller that keeps or filters them.
    pub(crate) fn into_vec(self) -> Vec<Task> {
        self.tasks
    }

    /// A store that a test builds from tasks it made.
    #[cfg(test)]
    pub(crate) fn from_vec_for_test(tasks: Vec<Task>) -> Self {
        Self {
            tasks,
            written_at: WRITES.with(std::cell::Cell::get),
        }
    }
}

/// The done and total task counts of `session_id` in a strict read of the store.
pub(crate) fn progress_in(tasks: &StrictTasks, session_id: &str) -> (u64, u64) {
    let mine: Vec<&Task> = tasks
        .as_slice()
        .iter()
        .filter(|t| t.session.as_deref() == Some(session_id))
        .collect();
    let done = mine.iter().filter(|t| t.state == TaskState::Done).count();
    (done as u64, mine.len() as u64)
}

/// [`progress_in`] for the session of `closed`, with `closed` counted in its saved state, not in
/// the state that the read holds. `task done` uses it so the hint does not depend on which
/// files the close wrote (#3482).
pub(crate) fn progress_after_close(tasks: &StrictTasks, closed: &Task) -> Option<(u64, u64)> {
    let session_id = closed.session.as_deref()?;
    let mine = tasks
        .as_slice()
        .iter()
        .filter(|t| t.session.as_deref() == Some(session_id));
    let (mut done, mut total) = (0, 0);
    for task in mine {
        let state = if task.slug == closed.slug {
            closed.state
        } else {
            task.state
        };
        total += 1;
        done += u64::from(state == TaskState::Done);
    }
    Some((done, total))
}

/// The title of the most recently updated `wip` or `waiting` task in `session_ids`, for the
/// statusline's "what's in progress right now" fill-in (#905). `None` when nothing matching is
/// currently `wip`/`waiting`, or when `session_ids` is empty. `tasks` is a strict read, so a
/// corrupt file never removes the title without a sign (#2687).
pub(crate) fn wip_title_in(tasks: &StrictTasks, session_ids: &[String]) -> Option<String> {
    most_recently_updated(tasks.as_slice().iter().filter(|t| {
        matches!(t.state, TaskState::Wip | TaskState::Waiting)
            && t.session
                .as_deref()
                .is_some_and(|sid| session_ids.iter().any(|s| s == sid))
    }))
    .map(|t| t.title.clone())
}

#[cfg(test)]
mod tests {
    use super::{progress_in, wip_title_in};
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
            let tasks = super::StrictTasks::from_vec_for_test(tasks);
            proptest::prop_assert_eq!(progress_in(&tasks, "a"), (done, mine.len() as u64));
        }

        /// The title is from a `wip` or `waiting` task of the named sessions, and no such task
        /// has a later `updated_at`. With no such task there is no title.
        #[test]
        fn the_title_is_the_latest_active_task_of_the_sessions(
            rows in proptest::collection::vec((0u8..3, 0u8..4, 0u8..10), 0..12),
        ) {
            let ids = ["a", "b", "c"];
            let states = [TaskState::Open, TaskState::Wip, TaskState::Waiting, TaskState::Done];
            let tasks: Vec<Task> = rows
                .iter()
                .enumerate()
                .map(|(i, (s, state, at))| {
                    let mut t = task(ids.get(usize::from(*s)).copied(), states[usize::from(*state)]);
                    t.title = format!("t{i}");
                    // Day 1 to 10, as the store writes them: RFC 3339, not a date alone (#3796).
                    t.updated_at = format!("2026-01-{:02}T00:00:00Z", at + 1);
                    t
                })
                .collect();
            let wanted = vec!["a".to_string(), "b".to_string()];
            let active: Vec<&Task> = tasks
                .iter()
                .filter(|t| {
                    matches!(t.state, TaskState::Wip | TaskState::Waiting)
                        && t.session.as_deref().is_some_and(|s| s == "a" || s == "b")
                })
                .collect();
            let strict = super::StrictTasks::from_vec_for_test(tasks.to_vec());
            match wip_title_in(&strict, &wanted) {
                None => proptest::prop_assert!(active.is_empty()),
                Some(title) => {
                    let chosen = active.iter().find(|t| t.title == title);
                    proptest::prop_assert!(chosen.is_some(), "{title} is not an active task");
                    // The oracle orders by instant, the same rule the title uses (#3796).
                    let latest = active
                        .iter()
                        .map(|t| t.updated_at.as_str())
                        .max_by(|a, b| crate::util::time::cmp_instants(a, b, "task updated_at"));
                    proptest::prop_assert_eq!(
                        chosen.map(|t| t.updated_at.as_str()),
                        latest
                    );
                }
            }
            proptest::prop_assert_eq!(wip_title_in(&strict, &[]), None);
        }
    }
}
