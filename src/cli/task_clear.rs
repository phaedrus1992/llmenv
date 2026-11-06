//! The `llmenv task clear` command. Deleted tasks cannot keep an `--other-session` note, so the
//! override is recorded on the owning session instead (#3481).

use std::path::Path;

use anyhow::Context;

use super::caller_session_for_task;
use super::task_guard::{guard_session, guard_tasks};
use crate::task::ownership::{
    Caller, OverrideNote, check_owner_less_delete, record_owner_less_delete,
};
use crate::task::session::append_override_note;

/// A task that passed the ownership check, with the override note when the check needed one.
type Checked = (String, Option<OverrideNote>);

/// `llmenv task clear`, run by the caller under the store lock, so the checked tasks are the
/// tasks that are deleted (#3478).
pub(super) fn run(
    state_dir: &Path,
    ids: &[String],
    session: Option<&str>,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let session_note = match session {
        Some(named) => guard_session(state_dir, named, other_session)?,
        None if ids.is_empty() => {
            anyhow::bail!("specify one or more task ids, or --session <id>")
        }
        None => None,
    };
    // `clear` deletes the task, so a pending override note has nowhere to live.
    let checked = guard_tasks(state_dir, ids, other_session)?;
    // Read the caller before the first delete. A read error after a delete would leave cleared
    // tasks with no note (#3481).
    let caller = caller_session_for_task(state_dir)?;
    if checked
        .iter()
        .any(|(_, note)| note.as_ref().is_some_and(OverrideNote::is_owner_less))
    {
        check_owner_less_delete(&caller)?;
    }
    let mut deleted = Vec::new();
    let outcome = match session {
        Some(named) => clear_session(state_dir, named, &mut deleted),
        None => clear_ids(state_dir, &checked, &mut deleted),
    };
    let plan = NotePlan {
        session,
        other_session,
        caller: &caller,
        checked: &checked,
        deleted: &deleted,
        session_note: session_note.as_deref(),
    };
    match (outcome, record_notes(state_dir, &plan)) {
        (Ok(()), note) => note,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(note)) => Err(error.context(format!(
            "the owning session's note was not saved either: {note:#}"
        ))),
    }
}

/// What the notes of one clear record, once the deletes have run.
struct NotePlan<'a> {
    session: Option<&'a str>,
    other_session: Option<&'a str>,
    caller: &'a Caller,
    checked: &'a [Checked],
    deleted: &'a [String],
    /// The note of `clear --session`, set when the override was used. It is kept even when no
    /// task was deleted (#3481).
    session_note: Option<&'a str>,
}

/// Records the notes of a clear: the deletes of tasks with no session on the caller's session,
/// and the `--other-session` override on the owning session.
fn record_notes(state_dir: &Path, plan: &NotePlan<'_>) -> anyhow::Result<()> {
    let owner_less: Vec<String> = plan
        .deleted
        .iter()
        .filter(|slug| {
            plan.checked.iter().any(|(c, note)| {
                c == *slug && note.as_ref().is_some_and(OverrideNote::is_owner_less)
            })
        })
        .cloned()
        .collect();
    if !owner_less.is_empty() {
        record_owner_less_delete(state_dir, plan.caller, &owner_less)?;
    }
    let Some(owner) = plan.other_session else {
        return Ok(());
    };
    let covered = overridden_slugs(plan.session, owner, plan.caller, plan.checked, plan.deleted);
    if !covered.is_empty() {
        return record_override(state_dir, owner, &plan.caller.describe(), &covered);
    }
    match (plan.session, plan.session_note) {
        (Some(named), Some(text)) => append_override_note(state_dir, named, Some(text)).with_context(|| {
            format!(
                "the session '{named}' was cleared, but its override note was not saved; do not \
                 retry the clear"
            )
        }),
        _ => Ok(()),
    }
}

/// The deleted slugs that the `--other-session` override covered. A task that the caller owned,
/// or that never needed the override, stays out of the note.
fn overridden_slugs(
    session: Option<&str>,
    owner: &str,
    caller: &Caller,
    checked: &[Checked],
    deleted: &[String],
) -> Vec<String> {
    match session {
        Some(named) => {
            let used = owner == named && !matches!(caller, Caller::Session(id) if id == named);
            if used { deleted.to_vec() } else { Vec::new() }
        }
        None => deleted
            .iter()
            .filter(|slug| {
                checked.iter().any(|(c, note)| {
                    c == *slug && note.as_ref().is_some_and(|n| !n.is_owner_less())
                })
            })
            .cloned()
            .collect(),
    }
}

/// Deletes every task of `named`, and records the deleted slugs. A failure stops the delete, so
/// the slugs that left the store are found by reading it again.
fn clear_session(state_dir: &Path, named: &str, deleted: &mut Vec<String>) -> anyhow::Result<()> {
    let before = session_slugs(state_dir, named)?;
    match crate::task::session::delete_tasks_in_session(state_dir, named) {
        Ok(cleared) => {
            println!("Cleared {} task(s) from session '{named}'", cleared.len());
            deleted.extend(cleared.into_iter().map(|task| task.slug));
            Ok(())
        }
        Err(error) => match session_slugs(state_dir, named) {
            Ok(remaining) => {
                deleted.extend(before.into_iter().filter(|slug| !remaining.contains(slug)));
                Err(error)
            }
            Err(read) => Err(error.context(format!(
                "some tasks may be deleted, and the store could not be read to note them: {read:#}"
            ))),
        },
    }
}

/// The slugs of the tasks tagged to session `named`.
fn session_slugs(state_dir: &Path, named: &str) -> anyhow::Result<Vec<String>> {
    Ok(crate::task::list_tasks_strict(state_dir)?
        .as_slice()
        .iter()
        .filter(|task| task.session.as_deref() == Some(named))
        .map(|task| task.slug.clone())
        .collect())
}

/// Deletes the checked tasks one by one. A failure stops the loop, and the error names the
/// tasks that are already gone.
fn clear_ids(
    state_dir: &Path,
    checked: &[Checked],
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
    append_override_note(state_dir, owner, Some(&text)).with_context(|| {
        format!(
            "the tasks were cleared, but the note on session '{owner}' was not saved; \
                 do not retry the clear"
        )
    })
}
