//! The one read of the task store that the reminders of a hook run share.

use std::path::Path;

use super::Task;

/// The tasks of `project` for a reminder. A session list or a task file that cannot be read is an
/// error, and the caller prints its text in place of the reminder. A silent skip would tell the
/// agent that nothing is open (#2608).
pub(super) fn store_for_reminder(
    state_dir: &Path,
    project: &str,
    stored: &Stored,
) -> Result<Vec<Task>, String> {
    let context = "the task store cannot be read";
    match (
        super::session::open_sessions_strict(state_dir, project),
        stored,
    ) {
        (Ok(_), Ok(tasks)) => Ok(tasks.to_vec()),
        (Err(e), _) => Err(store_warning_text(&e.context(context))),
        (_, Err(e)) => Err(store_warning_text(&anyhow::anyhow!("{context}: {e:#}"))),
    }
}

/// The task store of one hook run, read once and shared by its reminders (#3466).
pub(super) type Stored = anyhow::Result<Vec<Task>>;

/// The reminder text for a store that cannot be read.
pub(super) fn store_warning_text(error: &anyhow::Error) -> String {
    format!(
        "The task reminders were not checked ({error:#}). \
         Fix the task store, then run `llmenv task ls --all` to confirm."
    )
}
