//! The `llmenv task clear` command. Deleted tasks cannot keep an `--other-session` note, so the
//! override is recorded on the owning session instead (#3481).

use std::path::Path;

use anyhow::Context;

use super::caller_session_for_task;
use super::task_guard::{guard_session, guard_tasks};

/// `llmenv task clear`, run by the caller under the store lock, so the checked tasks are the
/// tasks that are deleted (#3478).
pub(super) fn run(
    state_dir: &Path,
    ids: &[String],
    session: Option<&str>,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(named) = session {
        guard_session(state_dir, named, other_session)?;
    }
    // `clear` deletes the task, so a pending override note has nowhere to live.
    let checked = guard_tasks(state_dir, ids, other_session)?;
    let mut deleted = Vec::new();
    if let Some(named) = session {
        let cleared = crate::task::session::delete_tasks_in_session(state_dir, named)?;
        println!("Cleared {} task(s) from session '{named}'", cleared.len());
        deleted.extend(cleared.into_iter().map(|task| task.slug));
    } else if ids.is_empty() {
        anyhow::bail!("specify one or more task ids, or --session <id>");
    } else {
        for (slug, _) in &checked {
            let task =
                crate::task::delete_task(state_dir, crate::task::TaskTarget::Resolved(slug))?;
            println!("Cleared task '{}' ({})", task.slug, task.title);
            deleted.push(task.slug);
        }
    }
    if let Some(owner) = other_session {
        record_override(state_dir, owner, &deleted)?;
    }
    Ok(())
}

/// Appends a note to the owning session that names the caller and the deleted tasks (#3481).
///
/// # Errors
/// Errors if the caller cannot be identified, or if the owning session cannot be written. The
/// deletes are already saved when this runs, so the error says so.
fn record_override(state_dir: &Path, owner: &str, deleted: &[String]) -> anyhow::Result<()> {
    let caller = caller_session_for_task(state_dir)?.describe();
    let text = format!(
        "Cleared with --other-session by {caller}. Deleted tasks: {}.",
        deleted.join(", ")
    );
    crate::task::session::update_resume(state_dir, owner, |resume| resume.append_note(&text))
        .map(|_| ())
        .with_context(|| {
            format!(
                "the tasks were cleared, but the note on session '{owner}' was not saved; \
                 do not retry the clear"
            )
        })
}
