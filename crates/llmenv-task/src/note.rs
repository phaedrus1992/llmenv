//! `llmenv task note`: a timestamped progress note on a task.

use std::path::Path;

use super::{
    Task, TaskNote, TaskTarget, load_task, now_rfc3339, save_task, touch_task_session,
    with_store_lock,
};

/// Append a timestamped progress note to a task. A [`TaskTarget::Resolved`] slug is not looked up
/// again (#3474, #3477).
pub fn note_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    text: &str,
) -> anyhow::Result<Task> {
    let target = target.into();
    let task = with_store_lock(state_dir, || {
        let slug = target.slug(state_dir)?;
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
