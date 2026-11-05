//! `llmenv task note`: a timestamped progress note on a task.

use std::path::Path;

use super::{
    Task, TaskNote, load_task, now_rfc3339, resolve_identifier, save_task, touch_task_session,
    with_store_lock,
};

/// Append a timestamped progress note to a task.
pub(crate) fn note_task(state_dir: &Path, input: &str, text: &str) -> anyhow::Result<Task> {
    note_with(state_dir, text, || resolve_identifier(state_dir, input))
}

/// [`note_task`] for a slug that the caller resolved in the same command, so the store is not
/// read again to find it (#3474). A slug with no task file is an error.
///
/// # Errors
/// The task cannot be loaded or saved.
pub(crate) fn note_resolved(state_dir: &Path, slug: &str, text: &str) -> anyhow::Result<Task> {
    note_with(state_dir, text, || Ok(slug.to_string()))
}

fn note_with(
    state_dir: &Path,
    text: &str,
    slug: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
        let slug = slug()?;
        let mut task = load_task(state_dir, &slug)?;
        task.notes.push(TaskNote {
            at: now_rfc3339(),
            text: text.to_string(),
        });
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}
