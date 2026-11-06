//! The ownership guard of the `llmenv task` commands: a task or a session of another
//! conversation is refused unless `--other-session` names its owner.

use super::caller_session_for_task;

/// Refuse a command that acts on task `id` when the task belongs to another session.
pub(super) fn guard_task(
    state_dir: &std::path::Path,
    id: &str,
    other_session: Option<&str>,
) -> anyhow::Result<(String, Option<crate::task::ownership::OverrideNote>)> {
    let caller = caller_session_for_task(state_dir)?;
    crate::task::ownership::ensure_task_is_ours(state_dir, id, &caller, other_session)
}

/// [`guard_task`] for several ids, with one read of the store. The command then acts on the
/// returned slugs, which are the tasks that were checked (#3477). Each command runs the check
/// and the change in one `with_store_lock`, so no other process changes the task between the
/// two (#3478).
pub(super) fn guard_tasks(
    state_dir: &std::path::Path,
    ids: &[String],
    other_session: Option<&str>,
) -> anyhow::Result<Vec<(String, Option<crate::task::ownership::OverrideNote>)>> {
    // No id means nothing to check, so the caller lookup does not run before the usage error.
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let caller = caller_session_for_task(state_dir)?;
    let inputs: Vec<&str> = ids.iter().map(String::as_str).collect();
    crate::task::ownership::ensure_tasks_are_ours(state_dir, &inputs, &caller, other_session)
}

/// Refuse `clear --session <id>` when that session is not the caller's.
pub(super) fn guard_session(
    state_dir: &std::path::Path,
    id: &str,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let caller = caller_session_for_task(state_dir)?;
    crate::task::ownership::ensure_session_is_ours(id, &caller, other_session)
}

/// Appends the pending `--other-session` note, once the command has succeeded.
pub(super) fn record_override(
    state_dir: &std::path::Path,
    audit: Option<crate::task::ownership::OverrideNote>,
) -> anyhow::Result<()> {
    match audit {
        Some(note) => note.record(state_dir),
        None => Ok(()),
    }
}
