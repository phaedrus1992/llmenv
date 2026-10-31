//! How well the current project's work is tracked, for the task nudges and the commit gate
//! (#2456).

use std::path::Path;

use super::{Task, TaskState, list_tasks_strict, project, queue_order, session};

/// How well the current project's work is tracked right now (#2456).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tracking {
    /// The project or the task store cannot be read, so no statement is possible. The text is
    /// the cause, for a message that names it.
    Unknown(String),
    /// No session is open for the project.
    NoSession,
    /// A session is open, but none of its tasks is unfinished.
    NoTasks,
    /// A session has unfinished tasks. `wip` is the slug of one task in progress, if any,
    /// `waiting` lists the slugs of the tasks that wait on the user, and `next` is the first
    /// task still `open`, the one to start next.
    Tracked {
        wip: Option<String>,
        waiting: Vec<String>,
        next: Option<String>,
    },
}

/// The tracking state of the current project for the conversation `caller`, over every open
/// session of the project that [`session::Session::visible_to`] the caller: a task in progress in
/// any of them counts. A session that another conversation owns does not (#3185). A read error is
/// [`Tracking::Unknown`], so a hook never nudges on a guess.
pub(crate) fn tracking(state_dir: &Path, caller: Option<&str>) -> Tracking {
    let project = match project::current_tag() {
        Ok(project) => project,
        Err(e) => {
            tracing::error!("project::current_tag failed, so tracking is unknown: {e:#}");
            return Tracking::Unknown(format!("the project cannot be named: {e:#}"));
        }
    };
    // A store that cannot be read is unknown, not empty: a hook never nudges or denies on a guess.
    let (sessions, tasks) = match (
        session::open_sessions_strict(state_dir, &project),
        list_tasks_strict(state_dir),
    ) {
        (Ok(sessions), Ok(tasks)) => (sessions, tasks),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("task store cannot be read, so tracking is unknown: {e:#}");
            return Tracking::Unknown(format!("{e:#}"));
        }
    };
    let ids: Vec<String> = sessions
        .into_iter()
        .filter(|s| s.visible_to(caller))
        .map(|s| s.id)
        .collect();
    if ids.is_empty() {
        return Tracking::NoSession;
    }
    let unfinished: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.state != TaskState::Done)
        .filter(|t| t.session.as_ref().is_some_and(|s| ids.contains(s)))
        .collect();
    if unfinished.is_empty() {
        return Tracking::NoTasks;
    }
    let wip = unfinished
        .iter()
        .find(|t| t.state == TaskState::Wip)
        .map(|t| t.slug.clone());
    let waiting = unfinished
        .iter()
        .filter(|t| t.state == TaskState::Waiting)
        .map(|t| t.slug.clone())
        .collect();
    // The queue runs in creation order, so the next task is the earliest open one.
    let next = unfinished
        .iter()
        .filter(|t| t.state == TaskState::Open)
        .min_by(|a, b| queue_order(a, b))
        .map(|t| t.slug.clone());
    Tracking::Tracked { wip, waiting, next }
}
