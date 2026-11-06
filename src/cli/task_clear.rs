//! The `llmenv task clear` command. Deleted tasks cannot keep an `--other-session` note, so the
//! override is recorded on the owning session instead (#3481).

use std::path::Path;

use anyhow::Context;

use super::caller_session_for_task;
use super::task_guard::{guard_session, guard_tasks};
use crate::task::ownership::Caller;

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
    } else if ids.is_empty() {
        anyhow::bail!("specify one or more task ids, or --session <id>");
    }
    // `clear` deletes the task, so a pending override note has nowhere to live.
    let checked = guard_tasks(state_dir, ids, other_session)?;
    // Read the caller before the first delete. A read error after a delete would leave cleared
    // tasks with no note (#3481).
    let caller = other_session
        .map(|_| caller_session_for_task(state_dir))
        .transpose()?;
    let mut deleted = Vec::new();
    let outcome = match session {
        Some(named) => clear_session(state_dir, named, &mut deleted),
        None => clear_ids(state_dir, &checked, &mut deleted),
    };
    let note = match (other_session, caller, session) {
        (Some(owner), Some(caller), Some(named)) if !deleted.is_empty() => {
            let used = owner == named && !matches!(&caller, Caller::Session(id) if id == named);
            used.then(|| record_override(state_dir, owner, &caller.describe(), &deleted))
        }
        (Some(owner), Some(caller), None) if !deleted.is_empty() => {
            let used = checked.iter().any(|(_, note)| note.is_some());
            used.then(|| record_override(state_dir, owner, &caller.describe(), &deleted))
        }
        _ => None,
    };
    match (outcome, note.unwrap_or(Ok(()))) {
        (Ok(()), note) => note,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(note)) => Err(error.context(format!(
            "the owning session's note was not saved either: {note:#}"
        ))),
    }
}

/// Deletes every task of `named`, and records the deleted slugs.
fn clear_session(state_dir: &Path, named: &str, deleted: &mut Vec<String>) -> anyhow::Result<()> {
    let cleared = crate::task::session::delete_tasks_in_session(state_dir, named)?;
    println!("Cleared {} task(s) from session '{named}'", cleared.len());
    deleted.extend(cleared.into_iter().map(|task| task.slug));
    Ok(())
}

/// Deletes the checked tasks one by one. A failure stops the loop, and the error names the
/// tasks that are already gone.
fn clear_ids(
    state_dir: &Path,
    checked: &[(String, Option<crate::task::ownership::OverrideNote>)],
    deleted: &mut Vec<String>,
) -> anyhow::Result<()> {
    for (slug, _) in checked {
        let task = crate::task::delete_task(state_dir, crate::task::TaskTarget::Resolved(slug))
            .with_context(|| {
                format!(
                    "the clear stopped at task '{slug}'; {} task(s) before it were cleared and \
                     are not restored",
                    deleted.len()
                )
            })?;
        println!("Cleared task '{}' ({})", task.slug, task.title);
        deleted.push(task.slug);
    }
    Ok(())
}

/// Appends a note to the owning session that names the caller and the deleted tasks (#3481).
///
/// # Errors
/// Errors if the owning session cannot be written. The deletes are already saved when this runs,
/// so the error says so.
fn record_override(
    state_dir: &Path,
    owner: &str,
    caller: &str,
    deleted: &[String],
) -> anyhow::Result<()> {
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
