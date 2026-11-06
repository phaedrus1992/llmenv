//! `llmenv task session finish`, and the untag step that a finish and an abandon share.

use std::path::Path;

use anyhow::Context;

use super::{Session, load_existing_session, save_session, try_tasks_in_session};
use crate::{Task, TaskNote, TaskState, now_rfc3339};

/// Clear the `session` tag from every task of `tasks` that is not `done`
/// and append `note` to it. `tasks` are the tasks of `session_id`, from the caller's strict read,
/// so this reads nothing (#3479). Caller must already hold the store lock. Returns
/// the untagged tasks. Already-`done` tasks keep their tag as history.
pub(super) fn untag_unfinished(
    state_dir: &Path,
    session_id: &str,
    tasks: Vec<Task>,
    now: &str,
    note: &str,
) -> anyhow::Result<Vec<Task>> {
    let mut untagged = Vec::new();
    for mut task in tasks.into_iter().filter(|t| t.state != TaskState::Done) {
        task.notes.push(TaskNote {
            at: now.to_string(),
            text: note.to_string(),
        });
        task.session = None;
        task.updated_at = now.to_string();
        crate::save_task(state_dir, &task).with_context(|| {
            format!(
                "untagging task '{}' from session '{session_id}'; {} earlier task(s) are already untagged",
                task.slug,
                untagged.len()
            )
        })?;
        untagged.push(task);
    }
    Ok(untagged)
}

/// What [`finish_session`] did.
#[derive(Debug, Clone)]
pub struct FinishOutcome {
    pub session: Session,
    /// Tasks that were `done` when the session closed.
    pub done: u64,
    /// Tasks tagged to the session before it closed, abandoned ones included.
    pub total: u64,
    /// Tasks untagged because `abandon_open` was set.
    pub abandoned: Vec<Task>,
}

fn unfinished_error(id: &str, unfinished: &[&Task]) -> anyhow::Error {
    let list: String = unfinished
        .iter()
        .map(|t| format!("\n  {} {} {}", t.state.as_str(), t.slug, t.title))
        .collect();
    anyhow::anyhow!(
        "session '{session}' has {n} unfinished task(s):{list}\nFinish them with `llmenv task done \
         <slug>`, drop one with `llmenv task clear <slug>`, or pass --abandon-open to untag \
         them and finish the session anyway.",
        session = id,
        n = unfinished.len()
    )
}

/// Finish an open session by id: stamps `finished_at`. A session with an
/// `open`, `wip`, or `waiting` task is not finished (#2416), so it is refused
/// unless `abandon_open` untags those tasks first.
///
/// # Errors
/// Errors if `id` doesn't resolve to an existing, currently-open session, or if
/// the session has unfinished tasks and `abandon_open` is false.
pub fn finish_session(
    state_dir: &Path,
    id: &str,
    abandon_open: bool,
) -> anyhow::Result<FinishOutcome> {
    crate::with_store_lock(state_dir, || {
        let mut session = load_existing_session(state_dir, id)?;
        if !session.is_open() {
            anyhow::bail!("session '{id}' is already closed");
        }
        // One strict read gives the unfinished check and the tasks to untag (#3479).
        let tasks = try_tasks_in_session(state_dir, &session.id)?;
        // The tasks that keep a session from finishing: every `open`, `wip`, or `waiting` task
        // (#2416).
        let unfinished: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.state != TaskState::Done)
            .collect();
        if !unfinished.is_empty() && !abandon_open {
            return Err(unfinished_error(id, &unfinished));
        }
        let total = tasks.len() as u64;
        let done = (tasks.len() - unfinished.len()) as u64;
        let now = now_rfc3339();
        let note = format!(
            "Dropped: session '{}' finished (`session finish --abandon-open`) before this task \
             was finished.",
            session.id
        );
        let abandoned = untag_unfinished(state_dir, &session.id, tasks, &now, &note)?;
        session.finished_at = Some(now);
        save_session(state_dir, &session).with_context(|| {
            format!(
                "session '{id}' stays open, but {} unfinished task(s) are already untagged from it",
                abandoned.len()
            )
        })?;
        Ok(FinishOutcome {
            session,
            done,
            total,
            abandoned,
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
