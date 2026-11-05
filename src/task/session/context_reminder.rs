//! The Stop reminder for an open session with unfinished tasks and no resume context (#2339).

use super::Session;
use crate::task::TaskState;
use crate::task::reminder_store::Stored;

/// Stop text for each open session in `project` that has unfinished tasks and nothing recorded
/// that tells a fresh agent what the work is (#2339). Does not presume the session is the
/// reader's own (#1028).
#[must_use]
pub(in crate::task) fn missing_context_reminders(
    project: &str,
    caller: Option<&str>,
    stored: &Stored,
) -> String {
    let sessions = match stored.open_sessions_visible_to(project, caller) {
        Ok(sessions) => sessions,
        Err(e) => return crate::task::store_warning_text(&e),
    };
    sessions
        .iter()
        .filter(|session| session.resume.needs_nudge())
        .filter_map(|session| match stored.tasks() {
            Ok(tasks)
                if tasks.as_slice().iter().any(|task| {
                    task.session.as_deref() == Some(session.id.as_str())
                        && task.state != TaskState::Done
                }) =>
            {
                Some(missing_context_line(session))
            }
            Ok(_) => None,
            // The unfinished check needs the whole session. A partial read cannot clear it, so the
            // line says the session was not checked (#2608).
            Err(e) => Some(format!(
                "Session '{label}' ({id}) was not checked for resume context: the task store cannot \
                 be read ({err:#}). Fix the task store, then run `llmenv task ls --all` to confirm.",
                label = session.name.as_deref().unwrap_or(session.id.as_str()),
                id = session.id,
                err = e,
            )),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The reminder for one open session that has no resume context.
fn missing_context_line(session: &Session) -> String {
    let id = &session.id;
    format!(
        "Session '{label}' ({id}) has no resume context. If you recognize it as your \
         own, run `llmenv task session edit {id} --context \"...\" --issue N` so a \
         fresh agent can pick it up after /clear. If you don't recognize it, it \
         belongs to a different session — leave it alone.",
        label = session.name.as_deref().unwrap_or(id.as_str()),
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, clippy::panic, reason = "test code")]
mod tests {
    use super::missing_context_reminders;
    use crate::task::reminder_store::Stored;
    use crate::task::session::{StartDecision, StartOutcome, start_session};
    use crate::task::{ParentSpec, add_task_for_session, complete_task};

    fn session_in(dir: &std::path::Path, project: &str) -> String {
        let outcome = start_session(dir, None, None, project, StartDecision::New).expect("start");
        let StartOutcome::Created(session) = outcome else {
            panic!("expected Created");
        };
        session.id
    }

    #[test]
    fn a_session_with_open_tasks_and_no_context_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = session_in(dir.path(), "proj-3466");
        add_task_for_session(dir.path(), "Step", ParentSpec::Detached, &id).expect("add");
        let stored = Stored::read(dir.path());
        let text = missing_context_reminders("proj-3466", None, &stored);
        assert!(
            text.contains(&format!("({id}) has no resume context")),
            "{text}"
        );
    }

    #[test]
    fn a_session_whose_tasks_are_all_done_is_not_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = session_in(dir.path(), "proj-3466");
        let task =
            add_task_for_session(dir.path(), "Step", ParentSpec::Detached, &id).expect("add");
        complete_task(dir.path(), &task.slug, true).expect("done");
        let stored = Stored::read(dir.path());
        assert_eq!(missing_context_reminders("proj-3466", None, &stored), "");
    }

    #[test]
    fn an_unreadable_store_says_the_session_was_not_checked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = session_in(dir.path(), "proj-3466");
        let sessions = crate::task::session::list_sessions_strict(dir.path());
        let stored = Stored::from_parts(Err(anyhow::anyhow!("bad task file")), sessions);
        let text = missing_context_reminders("proj-3466", None, &stored);
        assert!(
            text.contains(&format!("({id}) was not checked for resume context"))
                && text.contains("bad task file"),
            "{text}"
        );
    }
}
