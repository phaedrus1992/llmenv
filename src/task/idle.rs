//! The Stop reminder for an open session that has tasks queued and none in progress.

use std::collections::HashMap;

use super::session::{self, Session};
use super::{
    Stored, Task, TaskState, blockers, empty_session_lines, execution_order, for_current_project,
    relation, startability, store_for_reminder, store_warning_text,
};

/// An open session that holds `open` tasks while none of its tasks is `wip`
/// or `waiting`: work is planned, but nothing is in progress (#2338).
#[derive(Debug, Clone)]
pub(super) struct IdleSession {
    pub(super) session: session::Session,
    /// The task to start next: the first `open` task in execution order that `task start`
    /// accepts. `None` when the start refuses every open task, and `loop_error` then says why.
    pub(super) next: Option<Task>,
    pub(super) open_count: usize,
    /// Set when no task is actionable: the slug of the first open task that a blocker keeps from
    /// starting, and why (a loop in the blocker walk, or a blocker that is not done). The reminder
    /// names the reason instead of advising a start that cannot run (#3618, #3764).
    pub(super) loop_error: Option<(String, String)>,
}

/// Every open session for `project` that [`IdleSession`] describes. Kept
/// separate from the Stop wording so other end-of-turn checks can share it
/// (#2339). A `waiting` task makes a session quiet on purpose, so such a
/// session is not idle.
#[must_use]
pub(super) fn idle_sessions(open: Vec<Session>, all_tasks: &[Task]) -> Vec<IdleSession> {
    let by_slug: HashMap<&str, &Task> = all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    open.into_iter()
        .filter_map(|session| {
            let tasks: Vec<Task> = all_tasks
                .iter()
                .filter(|t| t.session.as_deref() == Some(session.id.as_str()))
                .cloned()
                .collect();
            if tasks
                .iter()
                .any(|t| matches!(t.state, TaskState::Wip | TaskState::Waiting))
            {
                return None;
            }
            let order = execution_order(&tasks);
            let open: Vec<&Task> = order
                .iter()
                .filter(|t| t.state == TaskState::Open)
                .collect();
            if open.is_empty() {
                return None;
            }
            let next = startability::first_startable(&open, all_tasks, &by_slug);
            let loop_error = match next {
                Some(_) => None,
                None => blockers::first_obstacle(&open, all_tasks, &by_slug),
            };
            Some(IdleSession {
                next: next.map(Task::to_owned),
                open_count: open.len(),
                loop_error,
                session,
            })
        })
        .collect()
}

/// The Stop reminder for each [`IdleSession`] in the current project. Like
/// [`session_finish_reminders`], it cannot tell whose session it names
/// (#1028), so the nudge is conditioned on the agent recognizing it.
pub(super) fn idle_session_reminders(
    caller: Option<&str>,
    stored: &Stored,
    end: TurnEnd,
) -> String {
    for_current_project(|project| idle_reminder_lines(project, caller, stored, end))
}

/// How the last assistant message ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    Statement,
    /// The message ends with a question or a request to the user.
    Question,
}

impl TurnEnd {
    /// The end of `message`, the `last_assistant_message` of a Stop payload. A missing message is
    /// a statement.
    pub(crate) fn of(message: Option<&str>) -> Self {
        if message.is_some_and(|m| m.trim_end().ends_with('?')) {
            Self::Question
        } else {
            Self::Statement
        }
    }
}

/// The line for an idle session with a task that the start accepts. After a question to the user
/// the `wait` command comes first, because the next step most likely needs the answer (#2473).
fn startable_line(idle: &IdleSession, next: &Task, end: TurnEnd) -> String {
    let id = &idle.session.id;
    let label = idle.session.name.as_deref().unwrap_or(id.as_str());
    let (count, slug, title) = (idle.open_count, &next.slug, &next.title);
    let commands = match end {
        TurnEnd::Question => format!(
            "You asked the user a question, so run `llmenv task wait {slug} \"<reason>\"` for \
             ({title}) while you wait for the answer. After the answer, or if that step needs \
             neither the user nor an outside event, run `llmenv task start {slug}`."
        ),
        TurnEnd::Statement => format!(
            "Run `llmenv task start {slug}` ({title}) before you work on that step. If that \
             step needs the user or an outside event first, run `llmenv task wait {slug} \
             \"<reason>\"` instead."
        ),
    };
    format!(
        "Session '{label}' ({id}) has {count} open task(s) and none in progress. If you \
         recognize this as your own session: {commands} The session cannot be closed while any \
         task is open. If you don't recognize it, it belongs to a different session — leave it \
         alone."
    )
}

pub(super) fn idle_reminder_lines(
    project: &str,
    caller: Option<&str>,
    stored: &Stored,
    end: TurnEnd,
) -> String {
    let tasks = match store_for_reminder(project, stored) {
        Ok(tasks) => tasks,
        Err(warning) => return warning,
    };
    let open = match stored.open_sessions_visible_to(project, caller) {
        Ok(open) => open,
        Err(e) => return store_warning_text(&e),
    };
    let open_ids: Vec<String> = open.iter().map(|s| s.id.clone()).collect();
    let stalled = relation::stalled_parent_lines(tasks.as_slice(), &open_ids);
    let idle = idle_sessions(open, tasks.as_slice())
        .iter()
        .map(|idle| {
            let id = &idle.session.id;
            let label = idle.session.name.as_deref().unwrap_or(id.as_str());
            if let Some((looped, why)) = &idle.loop_error {
                return format!(
                    "Session '{label}' ({id}) has {count} open task(s) and none in progress, but \
                     '{looped}' cannot start: {why}. If you don't recognize the session, it \
                     belongs to a different session — leave it alone.",
                    count = idle.open_count,
                );
            }
            let Some(next) = &idle.next else {
                return format!(
                    "Session '{label}' ({id}) has {count} open task(s) and none in progress, but \
                     the start refuses every one of them. Run `llmenv task ls` to see why.",
                    count = idle.open_count,
                );
            };
            startable_line(idle, next, end)
        })
        .collect::<Vec<_>>();
    empty_session_lines(stored, tasks.as_slice(), project, caller)
        .into_iter()
        .chain(stalled)
        .chain(idle)
        .collect::<Vec<_>>()
        .join("\n\n")
}
