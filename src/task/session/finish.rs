//! `llmenv task session finish`, and the untag step that a finish and an abandon share.

use std::path::Path;

use super::{Session, load_existing_session, save_session, try_tasks_in_session};
use crate::task::partial::PartialWrite;
use crate::task::{Task, TaskNote, TaskState, now_rfc3339};

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
    untag_with(session_id, tasks, now, note, |task| {
        crate::task::save_task(state_dir, task)
    })
}

/// [`untag_unfinished`] with the save as a parameter, so a test can fail one save. A failed save
/// returns a [`PartialWrite`] that holds the tasks already untagged.
fn untag_with(
    session_id: &str,
    tasks: Vec<Task>,
    now: &str,
    note: &str,
    mut save: impl FnMut(&Task) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<Task>> {
    let mut untagged = Vec::new();
    for mut task in tasks.into_iter().filter(|t| t.state != TaskState::Done) {
        task.notes.push(TaskNote {
            at: now.to_string(),
            text: note.to_string(),
        });
        task.session = None;
        task.updated_at = now.to_string();
        if let Err(e) = save(&task) {
            let source = e.context(format!(
                "cannot untag task '{}' from session '{session_id}'; already untagged: [{}]. \
                 Fix: run `llmenv task session finish {session_id} --abandon-open` again",
                task.slug,
                slugs(&untagged)
            ));
            return Err(PartialWrite::error(untagged, source));
        }
        untagged.push(task);
    }
    Ok(untagged)
}

fn slugs(tasks: &[Task]) -> String {
    tasks
        .iter()
        .map(|t| t.slug.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What [`finish_session`] did.
#[derive(Debug, Clone)]
pub(crate) struct FinishOutcome {
    pub(crate) session: Session,
    /// Tasks that were `done` when the session closed.
    pub(crate) done: u64,
    /// Tasks tagged to the session before it closed, abandoned ones included.
    pub(crate) total: u64,
    /// Tasks untagged because `abandon_open` was set.
    pub(crate) abandoned: Vec<Task>,
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
pub(crate) fn finish_session(
    state_dir: &Path,
    id: &str,
    abandon_open: bool,
) -> anyhow::Result<FinishOutcome> {
    crate::task::with_store_lock(state_dir, || {
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
        if let Err(e) = save_session(state_dir, &session) {
            let source = e.context(format!(
                "session '{id}' stays open, but these unfinished tasks are already untagged from \
                 it: [{}]. Fix: run `llmenv task session finish {id}` again",
                slugs(&abandoned)
            ));
            return Err(PartialWrite::error(abandoned, source));
        }
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
