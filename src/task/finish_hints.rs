//! Reminders for a session whose tasks are all done, and the hint `task done` prints for the
//! last one (#2987).

use std::path::Path;

use super::{Task, for_current_project, session, store_for_reminder, store_warning_text};

/// For every session open for the current project (resolved from the
/// process's actual cwd — hooks run with cwd set to the project directory)
/// whose tasks are all done, build a reminder nudging the agent to close it
/// out. Empty string if none qualify. A session whose task file cannot be read
/// adds a warning line, and the other sessions still get their reminders, so a
/// hook never blocks the agent.
///
/// Like [`wip_reminder`], this can't tell whether a fully-done session
/// belongs to the current conversation or a different, concurrently running
/// one (#1028) — closing out someone else's session is a real mutation of
/// their bookkeeping, so the nudge is conditioned on recognizing the session
/// rather than issued as a bare command.
pub(super) fn session_finish_reminders(
    state_dir: &Path,
    caller: Option<&str>,
    stored: &anyhow::Result<Vec<Task>>,
) -> String {
    for_current_project(
        |project| match store_for_reminder(state_dir, project, stored) {
            Ok(tasks) => finish_reminder_lines(state_dir, project, caller, &tasks),
            Err(warning) => warning,
        },
    )
}

fn finish_reminder_lines(
    state_dir: &Path,
    project: &str,
    caller: Option<&str>,
    tasks: &[Task],
) -> String {
    let open = match session::try_open_sessions_visible_to(state_dir, project, caller) {
        Ok(open) => open,
        Err(e) => return store_warning_text(&e),
    };
    let mut lines = Vec::new();
    for session in open {
        // The store was read strictly, so a task file that cannot be read never makes a
        // session look finished here.
        let (done, total) = super::progress_in(tasks, &session.id);
        if total == 0 || done < total {
            continue;
        }
        let label = session.name.as_deref().unwrap_or(session.id.as_str());
        lines.push(format!(
            "All {total} task(s) in session '{label}' ({id}) are done. If the work goes on \
             (next skill phase, review, push, PR, merge, CI), add that step now with \
             `llmenv task add <title> --session {id}` and continue. A question to the user \
             blocks only the step that needs the answer: run `llmenv task wait <slug> \
             \"<reason>\"` on that step and keep working on the others. A running CI job or \
             other external wait is not a stop point: poll it in this turn. If you recognize \
             this as your own session and nothing is left, run `llmenv task session finish \
             {id}`. If you don't recognize it, it belongs to a different session — leave it \
             alone.",
            id = session.id
        ));
    }
    lines.join("\n\n")
}

/// The hint `task done` prints when the closed task was the last one not done in its session
/// (#2987). The agent decides here whether the work goes on, so the hint names the next step.
/// `None` while the session still holds a task that is not done, or when the task has no session.
///
/// # Errors
/// Errors if the session's task files cannot be read.
pub(crate) fn last_task_hint(state_dir: &Path, task: &Task) -> anyhow::Result<Option<String>> {
    let Some(session_id) = task.session.as_deref() else {
        return Ok(None);
    };
    let (done, total) = session::try_session_progress(state_dir, session_id)?;
    if total == 0 || done < total {
        return Ok(None);
    }
    Ok(Some(
        "The session has no open task left. If the work goes on (next skill phase, review, \
         push, PR, merge, CI), add that step now with `llmenv task add <title>` and continue. \
         Finish the session only when nothing is left."
            .to_string(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::task::session::{StartDecision, StartOutcome, start_session};
    use crate::task::{ParentSpec, add_task_for_session, complete_task};

    /// A new session in the project the test runs from, matching the reminder's own lookup.
    fn new_session(dir: &Path) -> String {
        let project = crate::task::project::current_tag().expect("test");
        let outcome = start_session(dir, None, None, &project, StartDecision::New).expect("test");
        let StartOutcome::Created(session) = outcome else {
            panic!("expected Created");
        };
        session.id
    }

    #[test]
    fn last_task_hint_is_none_while_another_task_is_open() {
        let dir = TempDir::new().expect("test");
        let session_id = new_session(dir.path());
        let first = add_task_for_session(dir.path(), "Step 1", ParentSpec::Detached, &session_id)
            .expect("test");
        add_task_for_session(dir.path(), "Step 2", ParentSpec::Detached, &session_id)
            .expect("test");
        let done = complete_task(dir.path(), &first.slug, true).expect("test");
        assert_eq!(
            last_task_hint(dir.path(), &done.task).expect("test"),
            None,
            "one task is still open, so there is no hint"
        );
    }

    #[test]
    fn last_task_hint_names_the_next_step_after_the_last_task() {
        let dir = TempDir::new().expect("test");
        let session_id = new_session(dir.path());
        let only = add_task_for_session(dir.path(), "Only", ParentSpec::Detached, &session_id)
            .expect("test");
        let done = complete_task(dir.path(), &only.slug, true).expect("test");
        let hint = last_task_hint(dir.path(), &done.task)
            .expect("test")
            .expect("the last task must produce a hint");
        assert!(hint.contains("no open task left"));
        assert!(hint.contains("llmenv task add <title>"));
    }

    #[test]
    fn last_task_hint_is_none_for_a_task_without_a_session() {
        let dir = TempDir::new().expect("test");
        let session_id = new_session(dir.path());
        let only = add_task_for_session(dir.path(), "Only", ParentSpec::Detached, &session_id)
            .expect("test");
        let mut done = complete_task(dir.path(), &only.slug, true)
            .expect("test")
            .task;
        done.session = None;
        assert_eq!(last_task_hint(dir.path(), &done).expect("test"), None);
    }

    #[test]
    fn all_done_reminder_keeps_the_agent_working_through_waits_and_ci() {
        let dir = TempDir::new().expect("test");
        let session_id = new_session(dir.path());
        let only = add_task_for_session(dir.path(), "Only", ParentSpec::Detached, &session_id)
            .expect("test");
        complete_task(dir.path(), &only.slug, true).expect("test");

        let stored = super::super::list_tasks_strict(dir.path());
        let reminder = session_finish_reminders(dir.path(), None, &stored);
        assert!(
            reminder.contains(&session_id),
            "must name the session: {reminder}"
        );
        assert!(reminder.contains("llmenv task wait"), "{reminder}");
        assert!(reminder.contains("not a stop point"), "{reminder}");
        assert!(reminder.contains("session finish"), "{reminder}");
        assert!(
            reminder.find("llmenv task wait") < reminder.find("session finish"),
            "the question rule must come before the finish command: {reminder}"
        );
    }

    #[test]
    fn all_done_reminder_is_empty_for_a_session_with_no_tasks() {
        let dir = TempDir::new().expect("test");
        new_session(dir.path());
        let stored = super::super::list_tasks_strict(dir.path());
        assert_eq!(session_finish_reminders(dir.path(), None, &stored), "");
    }
}
