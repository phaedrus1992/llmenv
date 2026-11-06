//! Refuses task commands that act on another session's tasks (#2584). A bare
//! `--force` does not override this check. Only `--other-session <owner>` does,
//! and only when `<owner>` is the session that owns the task (#2591).

use std::path::Path;

use super::resolve::{check_identifier, resolve_in};
use super::session::{self, EngineIdentity, Session};
use super::{Task, TaskState};

/// Which session the calling conversation works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller {
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
pub(crate) fn caller_session(
    state_dir: &Path,
    project: &str,
    owner: &EngineIdentity,
) -> anyhow::Result<Caller> {
    // Strict read: a corrupt session file must not hide the open session that owns this caller (#2665).
    let open = session::open_sessions_strict(state_dir, project)?;
    Ok(match session::pick_open_session(open, owner) {
        // A caller with no conversation id is not the owner of an owned session. The write
        // guards must refuse it, as `task add` does (#3480).
        Ok(picked) if picked.may_take_tasks(owner.session_id.as_deref()) => {
            Caller::Session(picked.id)
        }
        Ok(picked) if owner.session_id.is_none() => Caller::Unidentified(format!(
            "the only open session '{}' belongs to another conversation, and this caller has no \
             conversation id. Set CLAUDE_CODE_SESSION_ID",
            picked.id
        )),
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
pub(crate) struct OverrideNote {
    slug: String,
    text: String,
    /// True for a task with no session. Such a task has no session to hold its note.
    owner_less: bool,
}

impl OverrideNote {
    /// Whether the task has no session, so a deletion of it is recorded on the caller's session.
    pub(crate) fn is_owner_less(&self) -> bool {
        self.owner_less
    }

    /// Appends the note to the task. The command change is already saved when this runs, so the
    /// error says so: a retry must not repeat the change.
    ///
    /// # Errors
    /// Errors if the task cannot be read or written.
    pub(crate) fn record(&self, state_dir: &Path) -> anyhow::Result<()> {
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
pub(crate) fn ensure_task_is_ours(
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
pub(crate) fn ensure_tasks_are_ours(
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
        // An owner-less task (an orphan of a replaced session) has no owner to refuse for. The
        // change is allowed, and the note names the caller so the change is on record (#3586).
        let text = format!(
            "acted on a task with no session; caller: {}",
            caller.describe()
        );
        return Ok(Some(OverrideNote {
            slug,
            text,
            owner_less: true,
        }));
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
            Ok(Some(OverrideNote {
                slug,
                text,
                owner_less: false,
            }))
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
/// Returns the note text when `--other-session` was needed and used. The caller appends it to
/// the session once the command has succeeded (#3596). Returns `None` when no override applied.
///
/// # Errors
/// Errors if the caller is another session and `other_session` does not name
/// `session_id`.
pub(crate) fn ensure_session_is_ours(
    session_id: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if matches!(caller, Caller::Session(id) if id == session_id) {
        return Ok(None);
    }
    match other_session {
        Some(named) if named == session_id => Ok(Some(format!(
            "acted on with --other-session {session_id}; caller: {}",
            caller.describe()
        ))),
        Some(named) => anyhow::bail!("this command acts on session '{session_id}', not '{named}'"),
        None => refuse_unless_caller(
            session_id,
            caller,
            &format!("this command acts on session '{session_id}'"),
        )
        .map(|()| None),
    }
}

/// Refuses a `task add --session <id>` that names a session of another conversation, unless
/// `--other-session` names that session's owner (#3486). A session with no recorded owner, or one
/// that the caller owns, takes the task without a check.
///
/// Returns `true` when the override was used, so the caller can record it.
///
/// # Errors
/// Errors if the session store cannot be read, or if the session belongs to another
/// conversation and `other_session` does not name its owner.
pub(crate) fn ensure_named_session_takes_tasks(
    state_dir: &Path,
    session_id: &str,
    owner: &EngineIdentity,
    other_session: Option<&str>,
) -> anyhow::Result<bool> {
    let sessions = session::list_sessions_strict(state_dir)?;
    // A missing or closed session is reported by the add resolver, with its own message.
    let Some(target) = sessions.iter().find(|s| s.id == session_id) else {
        return Ok(false);
    };
    if target.may_take_tasks(owner.session_id.as_deref()) {
        return Ok(false);
    }
    match other_session {
        Some(named) if named == session_id => Ok(true),
        _ => anyhow::bail!(foreign_session_message(session_id)),
    }
}

/// Refuses a task that would join `session` when the session belongs to another conversation,
/// or when the caller has no conversation id (#3480). The picker of a new task calls this, so
/// a sole open session of another conversation cannot take the task by default.
///
/// # Errors
/// Errors when [`Session::may_take_tasks`] refuses the caller.
pub(crate) fn check_takes_tasks(session: &Session, owner: &EngineIdentity) -> anyhow::Result<()> {
    if session.may_take_tasks(owner.session_id.as_deref()) {
        return Ok(());
    }
    anyhow::bail!(foreign_session_message(&session.id))
}

/// Refuses a parent link from a task of `child_session` to a parent in another session (#3480).
/// Starting a child promotes each open ancestor to `wip`, so a cross-session link would change
/// the other session's task. A parent with no session is allowed, as before sessions existed.
///
/// # Errors
/// Errors when `parent` belongs to a session other than `child_session`.
pub(crate) fn ensure_parent_in_session(
    parent: Option<&Task>,
    parent_slug: &str,
    child_session: Option<&str>,
) -> anyhow::Result<()> {
    // The caller resolved the name just before this check, so a missing task is a bug in the
    // caller. It must not pass as "no session to compare".
    let Some(parent) = parent else {
        anyhow::bail!(
            "parent task '{parent_slug}' was not found. Fix: run `llmenv task ls --all` to list the \
             task names, then pass one of them."
        );
    };
    match parent.session.as_deref() {
        None => Ok(()),
        Some(session) if Some(session) == child_session => Ok(()),
        Some(session) => anyhow::bail!(
            "task '{}' belongs to session '{session}', so it cannot be the parent of a task in \
             another session. Link a task of the same session, or none.",
            parent.slug
        ),
    }
}

/// A sub-task needs an unfinished parent in its own session.
pub(super) fn check_child_parent(parent: &Task, session_id: &str) -> anyhow::Result<()> {
    if parent.state == TaskState::Done {
        anyhow::bail!(
            "task '{}' is done, so it cannot take a sub-task. Reopen it with `llmenv task start \
             --reopen {}`.",
            parent.slug,
            parent.slug
        );
    }
    if parent.session.is_none() {
        anyhow::bail!(
            "task '{}' has no session, so it cannot take a sub-task of this one. Fix: pick a \
             parent task of session '{session_id}'.",
            parent.slug
        );
    }
    if parent.session.as_deref() != Some(session_id) {
        anyhow::bail!(
            "task '{}' belongs to another session, so it cannot take a sub-task of this one",
            parent.slug
        );
    }
    Ok(())
}

/// The refusal text for a session of another conversation. It names the one override.
fn foreign_session_message(session_id: &str) -> String {
    format!(
        "session '{session_id}' belongs to another conversation, so a task cannot be added to it. \
         To add it anyway, pass the owning session's id as --other-session <SESSION_ID>."
    )
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

/// Refuses the deletion of a task with no session when the caller has no session to record the
/// deletion on. Such a deletion would leave no record of the caller (#3586).
///
/// # Errors
/// Errors if the caller has no open session of its own.
pub(crate) fn check_owner_less_delete(caller: &Caller) -> anyhow::Result<()> {
    match caller {
        Caller::Session(_) => Ok(()),
        Caller::Unidentified(reason) => anyhow::bail!(
            "this command would delete a task with no session, and the caller has no session to \
             record the deletion on ({reason}). Fix: run the command from the conversation whose \
             session is open"
        ),
    }
}

/// Records the deletion of tasks with no session on the caller's session, which
/// [`check_owner_less_delete`] has already accepted. The deletes are saved when this runs.
///
/// # Errors
/// Errors if the caller has no session, or if the note cannot be written.
pub(crate) fn record_owner_less_delete(
    state_dir: &Path,
    caller: &Caller,
    slugs: &[String],
) -> anyhow::Result<()> {
    use anyhow::Context;
    let Caller::Session(id) = caller else {
        check_owner_less_delete(caller)?;
        return Ok(());
    };
    let text = format!(
        "Deleted tasks with no session: {}. Caller: {}.",
        slugs.join(", "),
        caller.describe()
    );
    session::append_override_note(state_dir, id, Some(&text)).with_context(|| {
        format!(
            "the tasks were deleted, but the note on session '{id}' was not saved; do not retry \
             the delete"
        )
    })
}
