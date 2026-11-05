//! The one read of the task store and the session files that the reminders of a hook run share.

use std::collections::HashSet;
use std::path::Path;

use super::StrictTasks;
use super::session::{self, Session, filter};

/// The task store and the session files of one hook run, each read once, strictly, and shared by
/// its reminders (#3466, #3468).
pub(super) struct Stored {
    tasks: anyhow::Result<StrictTasks>,
    sessions: anyhow::Result<Vec<Session>>,
}

/// The error of a shared read. The error is borrowed by every reminder, so only its text can be
/// carried on: the text is the `{:#}` form of the owned error, which is how every reminder
/// prints it. The source chain is not kept.
fn shared<T>(read: &anyhow::Result<T>) -> anyhow::Result<&T> {
    read.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))
}

impl Stored {
    /// Read the task store and the session files of `state_dir`.
    pub(super) fn read(state_dir: &Path) -> Self {
        Self {
            tasks: super::list_tasks_strict(state_dir),
            sessions: session::list_sessions_strict(state_dir),
        }
    }

    /// Every task of the store.
    ///
    /// # Errors
    /// The task store cannot be read.
    pub(super) fn tasks(&self) -> anyhow::Result<&StrictTasks> {
        shared(&self.tasks)
    }

    /// The open sessions of `project` that are not hidden from `caller`. `None` hides no
    /// session.
    ///
    /// # Errors
    /// The session files cannot be read.
    pub(super) fn open_sessions_visible_to(
        &self,
        project: &str,
        caller: Option<&str>,
    ) -> anyhow::Result<Vec<Session>> {
        Ok(filter::open_visible(
            shared(&self.sessions)?,
            project,
            caller,
        ))
    }

    /// The id of every session of `project`, open or closed, that is not hidden from `caller`.
    /// `None` gives every session of the project, so a caller always says which it means.
    ///
    /// # Errors
    /// The session files cannot be read.
    pub(super) fn session_ids_visible_to(
        &self,
        project: &str,
        caller: Option<&str>,
    ) -> anyhow::Result<HashSet<String>> {
        Ok(filter::ids_visible(
            shared(&self.sessions)?,
            project,
            caller,
        ))
    }

    /// A store from reads that a test made, such as a task read that failed.
    #[cfg(test)]
    pub(super) fn from_parts(
        tasks: anyhow::Result<StrictTasks>,
        sessions: anyhow::Result<Vec<Session>>,
    ) -> Self {
        Self { tasks, sessions }
    }
}

/// The tasks of `project` for a reminder. A session list or a task file that cannot be read is an
/// error, and the caller prints its text in place of the reminder. A silent skip would tell the
/// agent that nothing is open (#2608).
pub(super) fn store_for_reminder<'a>(
    project: &str,
    stored: &'a Stored,
) -> Result<&'a StrictTasks, String> {
    let context = "the task store cannot be read";
    stored
        .open_sessions_visible_to(project, None)
        .and_then(|_| stored.tasks())
        .map_err(|e| store_warning_text(&e.context(context)))
}

/// The reminder text for a store that cannot be read.
pub(super) fn store_warning_text(error: &anyhow::Error) -> String {
    format!(
        "The task reminders were not checked ({error:#}). \
         Fix the task store, then run `llmenv task ls --all` to confirm."
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, clippy::panic, reason = "test code")]
mod tests {
    use super::{Stored, store_for_reminder};
    use crate::task::session::{self, StartDecision, StartOutcome};

    fn start(dir: &std::path::Path, project: &str) -> String {
        let outcome =
            session::start_session(dir, None, None, project, StartDecision::New).expect("start");
        let StartOutcome::Created(created) = outcome else {
            panic!("expected Created");
        };
        created.id
    }

    #[test]
    fn the_open_sessions_are_those_of_the_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mine = start(dir.path(), "proj-3468");
        start(dir.path(), "other-3468");
        let stored = Stored::read(dir.path());
        let open = stored
            .open_sessions_visible_to("proj-3468", None)
            .expect("readable");
        let ids: Vec<&str> = open.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, [mine.as_str()]);
        let all = stored
            .session_ids_visible_to("proj-3468", None)
            .expect("readable");
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn a_session_read_error_reaches_the_reminder_text() {
        let tasks = Ok(crate::task::StrictTasks::from_vec_for_test(Vec::new()));
        let stored = Stored::from_parts(tasks, Err(anyhow::anyhow!("bad session file")));
        let err = store_for_reminder("proj-3468", &stored).expect_err("sessions are unreadable");
        assert!(
            err.contains("the task store cannot be read: bad session file"),
            "{err}"
        );
    }

    #[test]
    fn a_session_error_wins_over_a_task_error() {
        let stored = Stored::from_parts(
            Err(anyhow::anyhow!("bad task file")),
            Err(anyhow::anyhow!("bad session file")),
        );
        let err = store_for_reminder("proj-3468", &stored).expect_err("both reads failed");
        assert!(
            err.contains("bad session file") && !err.contains("bad task file"),
            "{err}"
        );
    }

    #[test]
    fn a_task_error_alone_names_the_task_store() {
        let stored = Stored::from_parts(Err(anyhow::anyhow!("bad task file")), Ok(Vec::new()));
        let err = store_for_reminder("proj-3468", &stored).expect_err("tasks are unreadable");
        assert!(
            err.contains("the task store cannot be read: bad task file"),
            "{err}"
        );
    }

    #[test]
    fn session_start_names_a_task_file_that_cannot_be_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tasks = crate::task::tasks_dir(dir.path());
        std::fs::create_dir_all(&tasks).expect("tasks dir");
        std::fs::write(tasks.join("broken.json"), "not json").expect("write");
        let text = crate::task::session_start_reminder(dir.path());
        assert!(text.contains("the task store cannot be read"), "{text}");
    }

    proptest::proptest! {
        /// A session read error reaches every session accessor with its text, for any caller.
        #[test]
        fn a_session_error_reaches_every_accessor(
            msg in "[a-z]{1,12}",
            caller in proptest::option::of("[a-z]{1,4}"),
        ) {
            let tasks = Ok(crate::task::StrictTasks::from_vec_for_test(Vec::new()));
            let stored = Stored::from_parts(tasks, Err(anyhow::anyhow!("{msg}")));
            let caller = caller.as_deref();
            let open = stored.open_sessions_visible_to("p", caller).expect_err("unreadable");
            let ids = stored.session_ids_visible_to("p", caller).expect_err("unreadable");
            let open = format!("{open:#}");
            proptest::prop_assert!(open.contains(&msg));
            let ids = format!("{ids:#}");
            proptest::prop_assert!(ids.contains(&msg));
        }
    }
}
