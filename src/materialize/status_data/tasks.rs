//! Task-tracker progress for the status line: the open sessions of one project.

use std::path::Path;

use crate::cli::statusline::data::{SessionProgress, SessionTasks, TasksData};

/// Task-tracker progress, scoped to the sessions open for the current project (mandatory-sessions
/// design). `session` is `None` when zero sessions are open for this project (the widget renders
/// empty); `Some` carries the summed `(done, total)` across every session open for this project.
/// A state dir or cwd that cannot be resolved is the `unreadable` marker, not an empty section, so
/// the status line never reads as "no work" (#2687).
pub(super) fn collect_tasks() -> TasksData {
    let unreadable = |what: &str, e: &dyn std::fmt::Display| {
        tracing::error!(
            "cannot read the tasks, {what} is unknown: {e}; the status line shows unreadable"
        );
        unreadable_tasks()
    };
    let state_dir = match crate::paths::state_dir() {
        Ok(dir) => dir,
        Err(e) => return unreadable("the state directory", &e),
    };
    match crate::task::project::current_tag() {
        Ok(project) => collect_tasks_from_state_dir(&state_dir, &project),
        Err(e) => unreadable("the project of the working directory", &e),
    }
}

/// Internal helper taking injectable state dir + project tag so tests can
/// exercise this without touching the real, ambient state dir or cwd.
pub(super) fn collect_tasks_from_state_dir(state_dir: &Path, project: &str) -> TasksData {
    // The status line has no text channel for a warning, so the error goes to the log. The
    // widget shows the `unreadable` marker, so a read failure never looks like "no work".
    match try_tasks_data(state_dir, project) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the tasks; the status line shows unreadable");
            unreadable_tasks()
        }
    }
}

/// The marker the status line shows when the tasks cannot be read.
fn unreadable_tasks() -> TasksData {
    TasksData {
        session: None,
        current: None,
        unreadable: true,
        sessions: Vec::new(),
    }
}

/// The task fields of the status line, read strictly: any unreadable session or task file is an
/// error. `sessions` lists each open session, in session-id order, so the status line can narrow
/// the counts to the conversation that reads it (#3196).
fn try_tasks_data(state_dir: &Path, project: &str) -> anyhow::Result<TasksData> {
    let mut open_sessions = crate::task::session::open_sessions_strict(state_dir, project)?;
    open_sessions.sort_by(|a, b| a.id.cmp(&b.id));
    let mut progress = Vec::with_capacity(open_sessions.len());
    for s in &open_sessions {
        progress.push(crate::task::session::try_session_progress(
            state_dir, &s.id,
        )?);
    }
    let session = if open_sessions.is_empty() {
        None
    } else {
        let (done, total) = progress
            .iter()
            .fold((0, 0), |acc, (d, t)| (acc.0 + d, acc.1 + t));
        Some(SessionProgress { done, total })
    };
    let session_ids: Vec<String> = open_sessions.iter().map(|s| s.id.clone()).collect();
    let current = crate::task::current_wip_title(state_dir, &session_ids)?;
    let mut sessions = Vec::with_capacity(open_sessions.len());
    for (s, (done, total)) in open_sessions.iter().zip(progress) {
        sessions.push(SessionTasks {
            owner: s.owner_session().map(str::to_owned),
            progress: SessionProgress { done, total },
            current: crate::task::current_wip_title(state_dir, std::slice::from_ref(&s.id))?,
        });
    }
    Ok(TasksData {
        session,
        current,
        unreadable: false,
        sessions,
    })
}
