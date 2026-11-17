//! The `llmenv task clear` command. Deleted tasks cannot keep an `--other-session` note, so the
//! override is recorded on the owning session instead (#3481).

use std::path::Path;

use anyhow::Context;

use super::caller_session_for_task;
use super::task_guard::guard_tasks;
use crate::task::ownership::{
    Caller, OverrideNote, SessionOverride, check_owner_less_delete, ensure_session_is_ours,
    record_cleared_override, record_owner_less_delete,
};

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
    if session.is_none() && ids.is_empty() {
        anyhow::bail!("specify one or more task ids, or --session <id>")
    }
    // One caller read serves both guards and the notes. It runs before the first delete, because
    // a read error after a delete would leave cleared tasks with no note (#3481).
    let caller = caller_session_for_task(state_dir)?;
    let session_note = match session {
        Some(named) => Some(ensure_session_is_ours(
            state_dir,
            named,
            &caller,
            other_session,
        )?),
        None => None,
    };
    // `clear` deletes the task, so a pending override note has nowhere to live.
    let checked = if ids.is_empty() {
        Vec::new()
    } else {
        guard_tasks(state_dir, &caller, ids, other_session)?.items
    };
    if checked
        .iter()
        .any(|(_, note)| note.as_ref().is_some_and(OverrideNote::is_owner_less))
    {
        check_owner_less_delete(&caller)?;
    }
    // The deletes leave no task to hold a note. The note of the override goes to the owning
    // session, and its journal exists before the first delete, so a crash leaves a trace (#3762).
    let _journal = journal_planned_clear(state_dir, other_session, &caller, &checked)?;
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
        session_note,
    };
    match (outcome, record_notes(state_dir, plan)) {
        (Ok(()), note) => note,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(note)) => Err(error.context(format!(
            "the owning session's note was not saved either: {note:#}"
        ))),
    }
}

/// The journal of the note for the tasks that a clear with `--other-session` is about to delete.
fn journal_planned_clear(
    state_dir: &Path,
    other_session: Option<&str>,
    caller: &Caller,
    checked: &[Checked],
) -> anyhow::Result<Option<crate::task::pending_note::Pending>> {
    let planned: Vec<&str> = checked
        .iter()
        .filter(|(_, note)| note.as_ref().is_some_and(|n| !n.is_owner_less()))
        .map(|(slug, _)| slug.as_str())
        .collect();
    let Some(owner) = other_session.filter(|_| !planned.is_empty()) else {
        return Ok(None);
    };
    let text = format!(
        "Cleared with --other-session by {}. Planned deletes: {}.",
        caller.describe(),
        planned.join(", ")
    );
    crate::task::pending_note::Pending::for_session(state_dir, owner, &text).map(Some)
}

/// What the notes of one clear record, once the deletes have run.
struct NotePlan<'a> {
    session: Option<&'a str>,
    other_session: Option<&'a str>,
    caller: &'a Caller,
    checked: &'a [Checked],
    deleted: &'a [String],
    /// The note of `clear --session`. It is kept even when no task was deleted (#3481).
    session_note: Option<SessionOverride>,
}

/// Records the notes of a clear: the deletes of tasks with no session on the caller's session,
/// and the `--other-session` override on the owning session.
fn record_notes(state_dir: &Path, plan: NotePlan<'_>) -> anyhow::Result<()> {
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
    // Both notes are tried, so a failed first note does not hide the second.
    let first = if owner_less.is_empty() {
        Ok(())
    } else {
        record_owner_less_delete(state_dir, plan.caller, &owner_less)
    };
    let second = record_override_notes(state_dir, plan);
    match (first, second) {
        (Ok(()), second) => second,
        (Err(first), Ok(())) => Err(first),
        (Err(first), Err(second)) => Err(first.context(format!(
            "the override note was not saved either: {second:#}"
        ))),
    }
}

/// Records the `--other-session` override on the owning session.
fn record_override_notes(state_dir: &Path, plan: NotePlan<'_>) -> anyhow::Result<()> {
    let Some(owner) = plan.other_session else {
        return Ok(());
    };
    let covered = overridden_slugs(plan.session, owner, plan.caller, plan.checked, plan.deleted);
    if !covered.is_empty() {
        return record_cleared_override(state_dir, owner, &plan.caller.describe(), &covered);
    }
    match (plan.session, plan.session_note) {
        (Some(named), Some(note)) => note.record(state_dir).with_context(|| {
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

/// Deletes every task of `named`, and records the deleted slugs. A failure stops the delete, and
/// the error holds the tasks that left the store before it, so the store is read once.
fn clear_session(state_dir: &Path, named: &str, deleted: &mut Vec<String>) -> anyhow::Result<()> {
    match crate::task::session::delete_tasks_in_session(state_dir, named) {
        Ok(cleared) => {
            println!("Cleared {} task(s) from session '{named}'", cleared.len());
            deleted.extend(cleared.into_iter().map(|task| task.slug));
            Ok(())
        }
        Err(error) => {
            deleted.extend(error.deleted.into_iter().map(|task| task.slug));
            Err(error.source)
        }
    }
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
