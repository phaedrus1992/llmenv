//! Refuses task commands that act on another session's tasks (#2584). A bare
//! `--force` does not override this check. Only `--other-session <owner>` does,
//! and only when `<owner>` is the session that owns the task (#2591).

use std::path::Path;

use super::Task;
use super::resolve::{check_identifier, resolve_in};
use super::session::{self, EngineIdentity};

/// Which session the calling conversation works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The one open session that this conversation may act for.
    Session(String),
    /// No session can be named. The text says why, for a refusal.
    Unidentified(String),
}

impl Caller {
    /// Names the caller for a task note.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Session(id) => format!("session '{id}'"),
            Self::Unidentified(reason) => format!("an unidentified caller ({reason})"),
        }
    }
}

/// Resolve the caller's session for `project`.
///
/// A session that another conversation owns never counts as the caller's, even
/// when it is the only open session. That case would otherwise let a second
/// conversation act on the first one's tasks.
///
/// # Errors
/// Errors if the session store cannot be read.
pub fn caller_session(
    state_dir: &Path,
    project: &str,
    owner: &EngineIdentity,
) -> anyhow::Result<Caller> {
    // Strict read: a corrupt session file must not hide the open session that owns this caller (#2665).
    let open = session::open_sessions_strict(state_dir, project)?;
    Ok(match session::pick_open_session(open, owner) {
        Ok(picked) if picked.visible_to(owner.session_id.as_deref()) => Caller::Session(picked.id),
        Ok(picked) => Caller::Unidentified(format!(
            "the only open session '{}' belongs to another conversation",
            picked.id
        )),
        Err(err) => Caller::Unidentified(
            err.ambiguity_message(
                "set CLAUDE_CODE_SESSION_ID, or pass --other-session <SESSION_ID>",
            )
            .unwrap_or_else(|| "no session of yours is open in this project".to_string()),
        ),
    })
}

/// The audit note for an `--other-session` override. The caller records it with
/// [`OverrideNote::record`] after the command succeeds, so a refused or failed
/// command leaves no note behind.
#[derive(Debug, PartialEq, Eq)]
pub struct OverrideNote {
    slug: String,
    text: String,
}

impl OverrideNote {
    /// Appends the note to the task. The command change is already saved when this runs, so the
    /// error says so: a retry must not repeat the change.
    ///
    /// # Errors
    /// Errors if the task cannot be read or written.
    pub fn record(&self, state_dir: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        super::note_task(state_dir, &self.slug, &self.text).with_context(|| {
            format!(
                "the change to task '{}' is saved, but its --other-session note is not. Fix: add \
                 the note with `llmenv task note {} <text>`, where the text is the next line \
                 (data, not a command):\n{}",
                self.slug, self.slug, self.text
            )
        })?;
        Ok(())
    }
}

/// [`ensure_tasks_are_ours`] for one name. It returns the resolved slug and the override note.
///
/// # Errors
/// The errors of [`ensure_tasks_are_ours`].
pub fn ensure_task_is_ours(
    state_dir: &Path,
    input: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<(String, Option<OverrideNote>)> {
    ensure_tasks_are_ours(state_dir, &[input], caller, other_session)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no task was checked for '{input}'"))
}

/// Refuse to act on each task of `inputs` that belongs to a session other than the
/// caller's. A task with no session is never refused.
///
/// `other_session` is the owning session id named by `--other-session`. The
/// override applies only when that id is the task's owner. It returns the note
/// that records the override. This function writes nothing itself.
///
/// All names share one strict read of the store. The result holds the resolved slug of each
/// name with its override note, in order, so the command acts on the tasks that were checked
/// and does not look the names up again (#3477).
///
/// # Errors
/// For the first name that fails: it does not resolve to a task, the store cannot be read, the
/// task belongs to another session and no matching `other_session` is given, or the named
/// `other_session` is not the owner.
pub fn ensure_tasks_are_ours(
    state_dir: &Path,
    inputs: &[&str],
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<Vec<(String, Option<OverrideNote>)>> {
    for input in inputs {
        check_identifier(input)?;
    }
    let all_tasks = super::list_tasks_strict(state_dir)?;
    inputs
        .iter()
        .map(|input| {
            let slug = resolve_in(state_dir, input, &all_tasks)?;
            let task = all_tasks
                .as_slice()
                .iter()
                .find(|t| t.slug == slug)
                .ok_or_else(|| anyhow::anyhow!("task '{slug}' is not in the task store"))?;
            let note = ownership_note(&slug, task, caller, other_session)?;
            Ok((slug, note))
        })
        .collect()
}

/// The override note for acting on `task`, or `None` when the caller may act on it.
fn ownership_note(
    slug: &str,
    task: &Task,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<Option<OverrideNote>> {
    let slug = slug.to_string();
    let Some(owner) = task.session.as_deref() else {
        return Ok(None);
    };
    if matches!(caller, Caller::Session(id) if id == owner) {
        return Ok(None);
    }
    match other_session {
        Some(named) if named == owner => {
            let text = format!(
                "acted on with --other-session {owner}; caller: {}",
                caller.describe()
            );
            Ok(Some(OverrideNote { slug, text }))
        }
        Some(named) => anyhow::bail!(
            "task '{slug}' belongs to session '{owner}', not '{named}'. \
             Pass the owning session's id to --other-session."
        ),
        None => refuse_unless_caller(
            owner,
            caller,
            &format!("task '{slug}' belongs to session '{owner}'"),
        )
        .map(|()| None),
    }
}

/// Refuse a command that acts on session `session_id` (`clear --session`, `session edit`,
/// `session note`, `session finish`) when the caller is not that session.
///
/// # Errors
/// Errors if the caller is another session and `other_session` does not name
/// `session_id`.
pub fn ensure_session_is_ours(
    session_id: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    if matches!(caller, Caller::Session(id) if id == session_id) {
        return Ok(());
    }
    match other_session {
        Some(named) if named == session_id => Ok(()),
        Some(named) => anyhow::bail!("this command acts on session '{session_id}', not '{named}'"),
        None => refuse_unless_caller(
            session_id,
            caller,
            &format!("this command acts on session '{session_id}'"),
        ),
    }
}

fn refuse_unless_caller(owner: &str, caller: &Caller, what: &str) -> anyhow::Result<()> {
    if matches!(caller, Caller::Session(id) if id == owner) {
        return Ok(());
    }
    let why = match caller {
        Caller::Session(id) => format!("your session is '{id}'"),
        Caller::Unidentified(reason) => reason.clone(),
    };
    anyhow::bail!(
        "{what}, and {why}. To act on it anyway, pass the owning session's id as \
         --other-session <SESSION_ID>."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
