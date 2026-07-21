//! Task sessions (#905): group tasks added while a session is active so
//! progress can be reported as "done/total" instead of a bare open-task
//! count.
//!
//! One JSON file per session under `<tasks_dir>/sessions/<id>.json`, plus a
//! single-line pointer file `<tasks_dir>/active_session` naming the
//! currently active session's id. Only one session can be active at a time —
//! [`start_session`] errors if one already is, unless `force` is set, in
//! which case the existing session is abandoned (see [`abandon_session`])
//! rather than silently dropped.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    Task, TaskNote, TaskState, list_tasks, now_rfc3339, slugify, task_path, tasks_dir, unique_slug,
};

/// A task session: a named (or anonymous) span of work that tasks created
/// during it are tagged with, so progress can be reported as done/total.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub name: Option<String>,
    /// RFC3339 timestamp.
    pub started_at: String,
    /// RFC3339 timestamp; `None` while the session is active.
    #[serde(default)]
    pub finished_at: Option<String>,
    /// RFC3339 timestamp; set instead of `finished_at` when a still-active
    /// session was superseded via `start_session(..., force: true)` rather
    /// than explicitly finished.
    #[serde(default)]
    pub abandoned_at: Option<String>,
}

fn sessions_dir(state_dir: &Path) -> PathBuf {
    tasks_dir(state_dir).join("sessions")
}

fn session_path(state_dir: &Path, id: &str) -> PathBuf {
    sessions_dir(state_dir).join(format!("{id}.json"))
}

fn active_pointer_path(state_dir: &Path) -> PathBuf {
    tasks_dir(state_dir).join("active_session")
}

fn save_session(state_dir: &Path, session: &Session) -> anyhow::Result<()> {
    std::fs::create_dir_all(sessions_dir(state_dir))?;
    let json = serde_json::to_string_pretty(session)?;
    crate::paths::write_owner_only_atomic(&session_path(state_dir, &session.id), json.as_bytes())?;
    Ok(())
}

fn load_session(state_dir: &Path, id: &str) -> anyhow::Result<Session> {
    let content = std::fs::read_to_string(session_path(state_dir, id))?;
    Ok(serde_json::from_str(&content)?)
}

/// The currently active session, if any. Performs no locking of its own — a
/// plain read, safe to call standalone (statusline collector, `session
/// show`) or from inside `super::with_store_lock` (e.g. `add_task` tagging a
/// new task), matching `list_tasks`' existing lock-free-read convention.
///
/// Tolerates a missing pointer file, a pointer whose session file is
/// deleted/corrupt, or a pointer left dangling by a `finish_session`/
/// `abandon_session` that wrote `finished_at`/`abandoned_at` but failed to
/// remove the pointer file — all treated as "no active session" rather than
/// erroring, since a degraded session pointer must never break a hook or the
/// statusline collector. A missing pointer file is expected (no session
/// ever started) and logged at nothing; an unreadable/corrupt pointer or
/// session file is not expected, so it's logged at `warn` even though it
/// still degrades to `None` — otherwise a corrupt state file would silently
/// bypass `start_session`'s one-session-at-a-time guard with no trace.
#[must_use]
pub fn active_session(state_dir: &Path) -> Option<Session> {
    let raw = match std::fs::read_to_string(active_pointer_path(state_dir)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                "active session pointer unreadable (treating as no active session): {e}"
            );
            return None;
        }
    };
    let id = raw.trim();
    if id.is_empty() {
        return None;
    }
    let session = match load_session(state_dir, id) {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(
                "active session pointer names '{id}' but its session file could not be \
                 loaded (treating as no active session): {e}"
            );
            return None;
        }
    };
    if session.finished_at.is_some() || session.abandoned_at.is_some() {
        return None;
    }
    Some(session)
}

/// Start a new session and make it active.
///
/// # Errors
/// Errors if a session is already active and `force` is `false` — finish it
/// first with [`finish_session`], or pass `force: true` to abandon it (see
/// [`abandon_session`]) and start the new one anyway.
pub fn start_session(state_dir: &Path, name: Option<&str>, force: bool) -> anyhow::Result<Session> {
    super::with_store_lock(state_dir, || {
        if let Some(existing) = active_session(state_dir) {
            if !force {
                anyhow::bail!(
                    "session '{}' is already active; finish it with `llmenv task session finish`, \
                     or pass --force to abandon it and start a new one",
                    existing.id
                );
            }
            abandon_session(state_dir, existing)?;
        }
        let dir = sessions_dir(state_dir);
        std::fs::create_dir_all(&dir)?;
        let base_slug = name
            .map(slugify)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "session".to_string());
        let id = unique_slug(&dir, &base_slug);
        let session = Session {
            id: id.clone(),
            name: name.map(str::to_string),
            started_at: now_rfc3339(),
            finished_at: None,
            abandoned_at: None,
        };
        save_session(state_dir, &session)?;
        crate::paths::write_owner_only_atomic(&active_pointer_path(state_dir), id.as_bytes())?;
        Ok(session)
    })
}

/// Finish the active session: stamps `finished_at` and clears the active
/// pointer. Unlike [`abandon_session`], this doesn't touch its tasks' `session`
/// tag — a deliberately finished session (even with incomplete tasks left in
/// it) is a legitimate historical record, not something to unwind.
///
/// # Errors
/// Errors if no session is currently active.
pub fn finish_session(state_dir: &Path) -> anyhow::Result<Session> {
    super::with_store_lock(state_dir, || {
        let mut session = active_session(state_dir)
            .ok_or_else(|| anyhow::anyhow!("no session is currently active"))?;
        session.finished_at = Some(now_rfc3339());
        save_session(state_dir, &session)?;
        std::fs::remove_file(active_pointer_path(state_dir))?;
        Ok(session)
    })
}

/// Every task currently tagged with `session_id`, in `list_tasks`' order.
fn tasks_in_session(state_dir: &Path, session_id: &str) -> Vec<Task> {
    list_tasks(state_dir)
        .into_iter()
        .filter(|t| t.session.as_deref() == Some(session_id))
        .collect()
}

/// Abandon `session` (force-superseded by a new one before it was finished):
/// stamps `abandoned_at`, and for every one of its tasks that isn't already
/// `done`, clears the `session` tag and appends a note recording the
/// orphaning — so the task falls back into the plain open/`wip` count
/// instead of staying invisibly attributed to a session nobody can track
/// progress on anymore, while leaving a visible trail of what happened.
/// Already-`done` tasks keep their tag untouched — they're a legitimate
/// historical record of what the session did accomplish.
///
/// Caller must already hold the store lock (called only from
/// [`start_session`], which owns `session` and doesn't reuse it after).
fn abandon_session(state_dir: &Path, mut session: Session) -> anyhow::Result<()> {
    let now = now_rfc3339();
    session.abandoned_at = Some(now.clone());
    save_session(state_dir, &session)?;

    let id = session.id.clone();
    let label = session.name.unwrap_or(session.id);
    for mut task in tasks_in_session(state_dir, &id)
        .into_iter()
        .filter(|t| t.state != TaskState::Done)
    {
        task.notes.push(TaskNote {
            at: now.clone(),
            text: format!(
                "Orphaned: session '{label}' was abandoned (a new session was force-started) \
                 before this task was finished."
            ),
        });
        task.session = None;
        task.updated_at = now.clone();
        super::save_task(state_dir, &task)?;
    }
    Ok(())
}

/// `(done, total)` counts for tasks tagged with `session_id` — the
/// statusline's "session active" progress numbers.
#[must_use]
pub fn session_progress(state_dir: &Path, session_id: &str) -> (u64, u64) {
    let tasks = tasks_in_session(state_dir, session_id);
    let done = tasks.iter().filter(|t| t.state == TaskState::Done).count() as u64;
    (done, tasks.len() as u64)
}

/// Delete every task tagged with `session_id` outright — for a batch of work
/// that's being deliberately thrown away, not just detached from a
/// superseded session (that's what force-`start_session` already does).
/// Returns the deleted tasks. Does not touch the session record itself or
/// the active pointer.
pub fn delete_tasks_in_session(state_dir: &Path, session_id: &str) -> anyhow::Result<Vec<Task>> {
    super::with_store_lock(state_dir, || {
        let tasks = tasks_in_session(state_dir, session_id);
        for t in &tasks {
            std::fs::remove_file(task_path(state_dir, &t.slug))?;
        }
        Ok(tasks)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::task::{add_task, done_task, load_task, save_task, start_task};
    use proptest::prelude::*;
    use tempfile::TempDir;

    #[test]
    fn active_session_none_when_never_started() {
        let dir = TempDir::new().expect("test");
        assert!(active_session(dir.path()).is_none());
    }

    #[test]
    fn start_session_makes_it_active() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        assert_eq!(session.name.as_deref(), Some("sprint 1"));
        assert!(session.finished_at.is_none());
        let active = active_session(dir.path()).expect("test");
        assert_eq!(active.id, session.id);
    }

    #[test]
    fn start_session_without_name_uses_generic_slug() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), None, false).expect("test");
        assert!(session.id.starts_with("session"));
    }

    #[test]
    fn start_session_errors_when_already_active_without_force() {
        let dir = TempDir::new().expect("test");
        start_session(dir.path(), Some("first"), false).expect("test");
        assert!(start_session(dir.path(), Some("second"), false).is_err());
    }

    #[test]
    fn finish_session_clears_active_pointer() {
        let dir = TempDir::new().expect("test");
        let started = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        let finished = finish_session(dir.path()).expect("test");
        assert_eq!(finished.id, started.id);
        assert!(finished.finished_at.is_some());
        assert!(active_session(dir.path()).is_none());
    }

    #[test]
    fn finish_session_errors_when_none_active() {
        let dir = TempDir::new().expect("test");
        assert!(finish_session(dir.path()).is_err());
    }

    #[test]
    fn a_new_session_can_start_after_the_previous_one_finishes() {
        let dir = TempDir::new().expect("test");
        start_session(dir.path(), Some("first"), false).expect("test");
        finish_session(dir.path()).expect("test");
        let second = start_session(dir.path(), Some("second"), false).expect("test");
        assert_eq!(second.name.as_deref(), Some("second"));
    }

    #[test]
    fn tasks_added_during_a_session_are_tagged_with_it() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        let task = add_task(dir.path(), "Do a thing", None).expect("test");
        assert_eq!(task.session, Some(session.id));
    }

    #[test]
    fn tasks_added_without_a_session_are_untagged() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do a thing", None).expect("test");
        assert!(task.session.is_none());
    }

    #[test]
    fn task_session_tag_is_fixed_at_creation_not_the_currently_active_session() {
        let dir = TempDir::new().expect("test");
        let first = start_session(dir.path(), Some("first"), false).expect("test");
        let task = add_task(dir.path(), "Do a thing", None).expect("test");
        finish_session(dir.path()).expect("test");
        start_session(dir.path(), Some("second"), false).expect("test");
        let reloaded = crate::task::load_task(dir.path(), &task.slug).expect("test");
        assert_eq!(reloaded.session, Some(first.id));
    }

    #[test]
    fn session_progress_counts_only_tasks_in_that_session() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        let t1 = add_task(dir.path(), "Task one", None).expect("test");
        let _t2 = add_task(dir.path(), "Task two", None).expect("test");
        finish_session(dir.path()).expect("test");
        // Added after the session finished — must not count toward it.
        add_task(dir.path(), "Unrelated task", None).expect("test");

        done_task(dir.path(), &t1.slug).expect("test");

        assert_eq!(session_progress(dir.path(), &session.id), (1, 2));
    }

    #[test]
    fn session_progress_empty_session_is_zero_of_zero() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("empty"), false).expect("test");
        assert_eq!(session_progress(dir.path(), &session.id), (0, 0));
    }

    #[test]
    fn active_session_ignores_dangling_pointer_to_deleted_session() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        std::fs::remove_file(session_path(dir.path(), &session.id)).expect("test");
        assert!(active_session(dir.path()).is_none());
    }

    #[test]
    fn active_session_degrades_on_corrupt_session_file_instead_of_panicking() {
        // Distinct from the dangling-pointer case above: the session file
        // exists but its content isn't valid JSON (e.g. a partial write from
        // a crash outside the atomic-rename path, or manual corruption).
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        std::fs::write(session_path(dir.path(), &session.id), b"not valid json").expect("test");
        assert!(active_session(dir.path()).is_none());
    }

    #[test]
    fn starting_multiple_sessions_over_time_each_get_distinct_ids() {
        let dir = TempDir::new().expect("test");
        let first = start_session(dir.path(), Some("sprint"), false).expect("test");
        finish_session(dir.path()).expect("test");
        let second = start_session(dir.path(), Some("sprint"), false).expect("test");
        assert_ne!(first.id, second.id);
    }

    // --- force-start / abandon (#905) ---

    #[test]
    fn force_start_abandons_the_active_session() {
        let dir = TempDir::new().expect("test");
        let first = start_session(dir.path(), Some("first"), false).expect("test");
        let second = start_session(dir.path(), Some("second"), true).expect("test");
        assert_ne!(first.id, second.id);

        let active = active_session(dir.path()).expect("test");
        assert_eq!(active.id, second.id);

        let abandoned = load_session(dir.path(), &first.id).expect("test");
        assert!(abandoned.abandoned_at.is_some());
        assert!(abandoned.finished_at.is_none());
    }

    #[test]
    fn force_start_with_no_active_session_behaves_like_a_plain_start() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("first"), true).expect("test");
        assert_eq!(session.name.as_deref(), Some("first"));
    }

    #[test]
    fn force_start_untags_and_notes_incomplete_tasks_from_the_abandoned_session() {
        let dir = TempDir::new().expect("test");
        start_session(dir.path(), Some("first"), false).expect("test");
        let open_task = add_task(dir.path(), "Still open", None).expect("test");
        let wip_task = add_task(dir.path(), "In progress", None).expect("test");
        start_task(dir.path(), &wip_task.slug).expect("test");

        start_session(dir.path(), Some("second"), true).expect("test");

        let reloaded_open = load_task(dir.path(), &open_task.slug).expect("test");
        assert!(reloaded_open.session.is_none());
        assert_eq!(reloaded_open.notes.len(), 1);
        assert!(reloaded_open.notes[0].text.contains("Orphaned"));
        assert!(reloaded_open.notes[0].text.contains("first"));

        let reloaded_wip = load_task(dir.path(), &wip_task.slug).expect("test");
        assert!(reloaded_wip.session.is_none());
        assert_eq!(
            reloaded_wip.state,
            TaskState::Wip,
            "state itself is untouched"
        );
    }

    #[test]
    fn force_start_leaves_done_tasks_tagged_to_the_abandoned_session() {
        let dir = TempDir::new().expect("test");
        let first = start_session(dir.path(), Some("first"), false).expect("test");
        let done_one = add_task(dir.path(), "Already finished", None).expect("test");
        done_task(dir.path(), &done_one.slug).expect("test");

        start_session(dir.path(), Some("second"), true).expect("test");

        let reloaded = load_task(dir.path(), &done_one.slug).expect("test");
        assert_eq!(reloaded.session, Some(first.id));
        assert!(
            reloaded.notes.is_empty(),
            "a done task is a legitimate historical record, not something to annotate"
        );
    }

    #[test]
    fn force_start_does_not_touch_tasks_from_other_sessions() {
        let dir = TempDir::new().expect("test");
        start_session(dir.path(), Some("first"), false).expect("test");
        let unrelated = add_task(dir.path(), "Unrelated task", None).expect("test");
        // Detach it from "first" manually to simulate a task from some
        // earlier, already-finished session — abandoning "first" must not
        // touch it.
        let mut t = load_task(dir.path(), &unrelated.slug).expect("test");
        t.session = Some("some-other-session".to_string());
        super::super::save_task(dir.path(), &t).expect("test");

        start_session(dir.path(), Some("second"), true).expect("test");

        let reloaded = load_task(dir.path(), &unrelated.slug).expect("test");
        assert_eq!(reloaded.session.as_deref(), Some("some-other-session"));
        assert!(reloaded.notes.is_empty());
    }

    // --- delete_tasks_in_session (#905) ---

    #[test]
    fn delete_tasks_in_session_removes_only_that_sessions_tasks() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("sprint 1"), false).expect("test");
        let in_session = add_task(dir.path(), "In the session", None).expect("test");
        finish_session(dir.path()).expect("test");
        let outside = add_task(dir.path(), "Outside the session", None).expect("test");

        let deleted = delete_tasks_in_session(dir.path(), &session.id).expect("test");
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].slug, in_session.slug);

        assert!(load_task(dir.path(), &in_session.slug).is_err());
        assert!(load_task(dir.path(), &outside.slug).is_ok());
    }

    #[test]
    fn delete_tasks_in_session_empty_session_deletes_nothing() {
        let dir = TempDir::new().expect("test");
        let session = start_session(dir.path(), Some("empty"), false).expect("test");
        let deleted = delete_tasks_in_session(dir.path(), &session.id).expect("test");
        assert!(deleted.is_empty());
    }

    proptest::proptest! {
        /// `(done, total)` must satisfy `done <= total`, `total` must equal
        /// exactly the tasks tagged with the session under test (tasks
        /// tagged with a *different* session, or untagged, must not leak
        /// in), and `done` must equal exactly the `Done`-state subset.
        #[test]
        fn session_progress_invariants_hold_for_arbitrary_task_mix(
            // Each entry: (tagged to the session under test, tagged to a
            // different session instead, is Done).
            states in proptest::collection::vec(
                (proptest::bool::ANY, proptest::bool::ANY, proptest::bool::ANY),
                0..12,
            ),
        ) {
            let dir = TempDir::new().expect("test");
            let session_id = "session-under-test";
            let mut expected_total = 0u64;
            let mut expected_done = 0u64;
            for (i, (tagged, other_session, done)) in states.iter().enumerate() {
                let session = if *tagged {
                    expected_total += 1;
                    if *done {
                        expected_done += 1;
                    }
                    Some(session_id.to_string())
                } else if *other_session {
                    Some("some-other-session".to_string())
                } else {
                    None
                };
                let task = Task {
                    slug: format!("task-{i}"),
                    title: format!("Task {i}"),
                    state: if *done { TaskState::Done } else { TaskState::Open },
                    parent: None,
                    blocked_on: Vec::new(),
                    notes: Vec::new(),
                    session,
                    created_at: now_rfc3339(),
                    updated_at: now_rfc3339(),
                };
                save_task(dir.path(), &task).expect("test");
            }
            let (done, total) = session_progress(dir.path(), session_id);
            prop_assert!(done <= total);
            prop_assert_eq!(total, expected_total);
            prop_assert_eq!(done, expected_done);
        }
    }
}
