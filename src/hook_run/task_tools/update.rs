//! The TaskUpdate redirect: one status change and one note, with the ownership check of the
//! `llmenv task` commands (#3478).

use std::path::Path;

use serde_json::Value;

use super::{deny, str_field};
use crate::task::{self, session};

/// Apply a TaskUpdate for the conversation `owner`, and return the deny text for the agent.
pub(super) fn update(
    input: Option<&Value>,
    state_dir: &Path,
    project: &str,
    owner: &session::EngineIdentity,
) -> String {
    // Claude Code may stream either `taskId` or the pre-rename `task_id`.
    let Some(id) = str_field(input, "taskId")
        .or_else(|| str_field(input, "task_id"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return deny(
            "TaskUpdate carried no `taskId`; update via `llmenv task start|note|done|wait <id>` \
             or `llmenv task block <id> --on <other>` (see `llmenv task ls --all` for ids).",
        );
    };
    let status = str_field(input, "status");
    let text = str_field(input, "description")
        .or_else(|| str_field(input, "subject"))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // A description/subject on an update becomes a progress note, but not on a delete, because
    // the task is gone. The deny says so, so the text is not lost without a word.
    let deleted = status == Some("deleted");
    let note = text.filter(|_| !deleted);
    let dropped = if deleted && text.is_some() {
        " (description not saved: the task is deleted)"
    } else {
        ""
    };
    // The check, the change, and the note run under one store lock, so the task that was checked
    // is the task that changes (#3478). A refused check saves no note.
    let applied = task::with_store_lock(state_dir, || {
        let slug = checked_slug(state_dir, project, owner, id)?;
        let change = change(state_dir, &slug, status);
        // The note is kept when the change fails too, and the deny says where it went.
        let note = note.map_or_else(String::new, |note| {
            note_outcome(
                task::note_task(state_dir, task::TaskTarget::Resolved(&slug), note),
                &slug,
            )
        });
        Ok((change, note))
    });
    match applied {
        Ok((Ok(msg), note)) => deny(&format!(
            "{msg}{note}{dropped} — llmenv task tracker (TaskUpdate redirected)."
        )),
        Ok((Err(e), note)) => failed(id, &e, &note),
        Err(e) => failed(id, &e, ""),
    }
}

/// The slug that `id` names, when the caller may change that task. The check is the check of
/// the `llmenv task` commands. TaskUpdate has no `--other-session`, so no override applies.
fn checked_slug(
    state_dir: &Path,
    project: &str,
    owner: &session::EngineIdentity,
    id: &str,
) -> anyhow::Result<String> {
    let caller = task::ownership::caller_session(state_dir, project, owner)?;
    let mut checked = task::ownership::ensure_tasks_are_ours(state_dir, &[id], &caller, None)?;
    checked
        .pop()
        .map(|(slug, _)| slug)
        .ok_or_else(|| anyhow::anyhow!("no task was checked for '{id}'"))
}

/// Run the status change on the checked `slug`, and return the text that names it.
fn change(state_dir: &Path, slug: &str, status: Option<&str>) -> anyhow::Result<String> {
    let target = task::TaskTarget::Resolved(slug);
    match status {
        // force=false: the redirect enforces the hard block on an unmet `blocked_on` (#1164)
        // that `llmenv task start` enforces, so the native tool is no bypass.
        Some("in_progress") => {
            task::start_task(state_dir, target, false).map(|t| format!("started '{}'", t.slug))
        }
        // force=false: the redirect keeps the start check of `llmenv task done` (#2416).
        Some("completed") => task::complete_task(state_dir, target, false)
            .map(|c| format!("completed '{}'", c.task.slug)),
        Some("deleted") => {
            task::delete_task(state_dir, target).map(|t| format!("deleted '{}'", t.slug))
        }
        // Name the unrecognized value, so an unknown status does not read as a change that ran.
        Some(other) => Ok(format!(
            "left '{slug}' unchanged (status '{other}' not recognized)"
        )),
        None => Ok(format!("left '{slug}' unchanged")),
    }
}

/// The deny for a TaskUpdate that did not apply. It keeps the real reason: an invalid id, no
/// match, an ambiguous prefix, a task of another session, or a refused transition.
fn failed(id: &str, e: &anyhow::Error, note: &str) -> String {
    tracing::error!(error = %format!("{e:#}"), id = %id, "task redirect: couldn't update task");
    deny(&format!(
        "couldn't update task '{id}' ({e:#}){note}. If the id is wrong, run \
         `llmenv task ls --all` for the current ids."
    ))
}

/// The text that tells the agent what happened to the note of a TaskUpdate.
fn note_outcome(saved: anyhow::Result<task::Task>, target: &str) -> String {
    match saved {
        Ok(task) => format!(" (note saved to '{}')", task.slug),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), target = %target, "task redirect: progress note not saved");
            format!(" (note not saved: {e:#})")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
