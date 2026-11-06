//! `llmenv task done`: close a task and print the notes the close leaves behind.

use std::path::Path;

use super::{caller_session_for_task, record_override};

/// Close task `id` and print what the close left behind: a skipped start, undone sub-tasks, or
/// the hint for the last open task in its session (#2987).
pub(super) fn run(
    state_dir: &Path,
    id: &str,
    force: bool,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    // The check and the close run under one store lock, so the checked task is the task that
    // closes (#3478).
    let completed = crate::task::with_store_lock(state_dir, || {
        // One caller read serves the check and the close note. It runs before the close, so a
        // failure here leaves the task open.
        let caller = caller_session_for_task(state_dir)?;
        let (slug, audit) =
            crate::task::ownership::ensure_task_is_ours(state_dir, id, &caller, other_session)?;
        let completed = crate::task::complete_task_by(
            state_dir,
            crate::task::TaskTarget::Resolved(&slug),
            force,
            &caller,
        )?;
        // The close is saved. A failed audit note says so in its error (OverrideNote::record),
        // and the notes of the close print first, so a failed audit does not hide them.
        let audit_result = record_override(state_dir, audit);
        Ok((completed, audit_result))
    })?;
    let (completed, audit_result) = completed;
    println!("Completed '{}'", completed.task.slug);
    if let Some(note) = completed.skipped_start_note() {
        println!("{note}");
    }
    if let Some(note) = completed.undone_children_note() {
        println!("{note}");
    }
    match crate::task::last_task_hint(state_dir, &completed.task) {
        Ok(Some(hint)) => println!("{hint}"),
        Ok(None) => {}
        // A failed hint does not undo a close that succeeded. The warning names the fix.
        Err(e) => println!(
            "Warning: '{}' is closed, but the next-step hint could not be read: {e:#}. Fix: run `llmenv task ls --session <id>`.",
            completed.task.slug
        ),
    }
    audit_result
}
