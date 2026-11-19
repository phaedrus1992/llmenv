//! Refuses task commands that act on another session's tasks (#2584). A bare
//! `--force` does not override this check. Only `--other-session <owner>` does,
//! and only when `<owner>` is the session that owns the task (#2591).

use std::path::Path;

use super::note_not_saved;
use super::pending_note::Pending;
use super::resolve::{check_identifier, resolve_in};
use super::session::{self, EngineIdentity, Session};
use super::{StrictTasks, Task, TaskState};

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
    #[must_use]
    pub fn describe(&self) -> String {
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
    Ok(caller_for_pick(
        &session::pick_open_session(open, owner),
        owner,
    ))
}

/// The caller that a pick of the open sessions means. A command that already picked a session
/// uses this, so it does not read the session store a second time (#3743).
pub fn caller_for_pick(
    pick: &Result<Session, session::PickError>,
    owner: &EngineIdentity,
) -> Caller {
    match pick {
        // A caller with no conversation id is not the owner of an owned session. The write
        // guards must refuse it, as `task add` does (#3480).
        Ok(picked) if picked.may_take_tasks(owner.session_id.as_deref()) => {
            Caller::Session(picked.id.clone())
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
        Err(session::PickError::NoneOpen) => {
            Caller::Unidentified("no session of yours is open in this project".to_string())
        }
        Err(err @ session::PickError::Ambiguous { .. }) => Caller::Unidentified(
            err.message("set CLAUDE_CODE_SESSION_ID, or pass --other-session <SESSION_ID>"),
        ),
    }
}

/// The audit note for an `--other-session` override. The caller records it with
/// [`OverrideNote::record`] after the command succeeds, so a refused or failed
/// command leaves no note behind.
#[derive(Debug)]
pub struct OverrideNote {
    slug: String,
    text: String,
    /// True for a task with no session. Such a task has no session to hold its note.
    owner_less: bool,
    /// The journal of the note, so a crash before [`OverrideNote::record`] leaves a trace (#3762).
    pending: Pending,
}

impl OverrideNote {
    /// Whether the task has no session, so a deletion of it is recorded on the caller's session.
    pub fn is_owner_less(&self) -> bool {
        self.owner_less
    }

    /// For a command that deletes the task. The task cannot hold the note, so the journal of the
    /// task note ends here. The note of an owner-less task goes to the caller's session, and the
    /// journal for that note is returned. Keep it until the note is written (#3762).
    ///
    /// # Errors
    /// The journal cannot be written. The command must stop before it deletes.
    pub fn journal_for_delete(
        &self,
        state_dir: &Path,
        caller: &Caller,
    ) -> anyhow::Result<Option<Pending>> {
        let journal = match (self.owner_less, caller) {
            (true, Caller::Session(id)) => {
                let text = format!(
                    "Deleted tasks with no session: {}. Caller: {}.",
                    self.slug,
                    caller.describe()
                );
                Some(Pending::for_session(state_dir, id, &text)?)
            }
            _ => None,
        };
        self.pending.finish();
        Ok(journal)
    }

    /// Appends the note to the task. The command change is already saved when this runs, so the
    /// error says so: a retry must not repeat the change.
    ///
    /// # Errors
    /// Errors if the task cannot be read or written.
    pub fn record(&self, state_dir: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let written = super::note_task(state_dir, &self.slug, &self.text);
        self.pending.finish();
        note_not_saved::mark_result(written.map(drop).with_context(|| {
            format!(
                "the change to task '{}' is saved, but its --other-session note is not. \
                         Fix: add the note with `llmenv task note {} <text>`, where the text is \
                         the next line (data, not a command):\n{}",
                self.slug, self.slug, self.text
            )
        }))
    }
}

/// [`ensure_tasks_are_ours`] for one name. It returns the resolved slug and the override note.
///
/// # Errors
/// The errors of [`ensure_tasks_are_ours`].
pub fn ensure_task_is_ours(
    state_dir: &Path,
    tasks: &StrictTasks,
    input: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<(String, Option<OverrideNote>)> {
    ensure_tasks_are_ours(state_dir, tasks, &[input], caller, other_session)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no task was checked for '{input}'"))
}

/// The strict read of the store for an ownership check. Each name is checked first, so a bad
/// name gets its own error even when the store is unreadable. The command passes the read to
/// the check and then to the change, so one command reads the store once (#3485).
///
/// # Errors
/// A name is not a valid task identifier, or the store cannot be read.
pub fn read_for_check(state_dir: &Path, inputs: &[&str]) -> anyhow::Result<StrictTasks> {
    check_names(inputs)?;
    super::list_tasks_strict(state_dir)
}

/// Refuse a name that cannot be a task slug, before any read of the store.
///
/// # Errors
/// A name is not a valid task identifier.
pub fn check_names(inputs: &[&str]) -> anyhow::Result<()> {
    inputs.iter().try_for_each(|input| check_identifier(input))
}

/// Refuse to act on each task of `inputs` that belongs to a session other than the
/// caller's. A task with no session is never refused.
///
/// `other_session` is the owning session id named by `--other-session`. The
/// override applies only when that id is the task's owner. It returns the note
/// that records the override. This function writes nothing itself.
///
/// All names share the strict read `all_tasks`, which the caller made with [`read_for_check`].
/// The result holds the resolved slug of each name with its override note, in order, so the
/// command acts on the tasks that were checked and does not look the names up again (#3477).
///
/// # Errors
/// For the first name that fails: it does not resolve to a task, the task belongs to another
/// session and no matching `other_session` is given, or the named `other_session` is not the
/// owner.
pub fn ensure_tasks_are_ours(
    state_dir: &Path,
    all_tasks: &StrictTasks,
    inputs: &[&str],
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<Vec<(String, Option<OverrideNote>)>> {
    // `resolve_in` needs checked names. The check is cheap, and a caller that did not use
    // `read_for_check` still gets it.
    check_names(inputs)?;
    inputs
        .iter()
        .map(|input| {
            let slug = resolve_in(state_dir, input, all_tasks)?;
            let task = all_tasks
                .as_slice()
                .iter()
                .find(|t| t.slug == slug)
                .ok_or_else(|| anyhow::anyhow!("task '{slug}' is not in the task store"))?;
            let note = ownership_note(state_dir, &slug, task, caller, other_session)?;
            Ok((slug, note))
        })
        .collect()
}

/// The override note for acting on `task`, or `None` when the caller may act on it.
fn ownership_note(
    state_dir: &Path,
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
        let pending = Pending::for_task(state_dir, &slug, &text)?;
        return Ok(Some(OverrideNote {
            slug,
            text,
            owner_less: true,
            pending,
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
            let pending = Pending::for_task(state_dir, &slug, &text)?;
            Ok(Some(OverrideNote {
                slug,
                text,
                owner_less: false,
                pending,
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

/// The audit note of a command that acts on a session of another conversation with
/// `--other-session` (#3596). Only [`ensure_session_is_ours`] makes one, so a note exists only for
/// a session that passed the guard. The command records it once its change has saved. A caller
/// that drops it gets a compiler warning (#3602).
#[must_use = "record the override note once the command has succeeded"]
#[derive(Debug)]
pub struct SessionOverride {
    session_id: String,
    /// `None` when the caller owns the session, so no override applied.
    text: Option<String>,
    /// The journal of the note, present when a note exists (#3762).
    pending: Option<Pending>,
}

impl SessionOverride {
    /// The note text, or `None` when no override applied.
    #[cfg(test)]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Append the note to the session. Nothing is written when no override applied. The command
    /// runs this after its change has saved, under the same store lock.
    ///
    /// # Errors
    /// Errors if the session cannot be read or written. The change itself is already saved.
    pub fn record(self, state_dir: &Path) -> anyhow::Result<()> {
        let written =
            session::append_override_note(state_dir, &self.session_id, self.text.as_deref());
        if let Some(pending) = &self.pending {
            pending.finish();
        }
        note_not_saved::mark_result(written)
    }

    /// Record the note for a command that failed after it had changed some tasks, then give back
    /// the command's error. Nothing is written when the error holds no changed task, because
    /// the command then changed nothing. A failed note is added to the error text (#3749).
    pub fn record_after_failure(self, state_dir: &Path, error: anyhow::Error) -> anyhow::Error {
        if super::partial::changed_before_failure(&error).is_empty() {
            return error;
        }
        let slugs: Vec<&str> = super::partial::changed_before_failure(&error)
            .iter()
            .map(|t| t.slug.as_str())
            .collect();
        let tasks = slugs.join(", ");
        match self.record(state_dir) {
            Ok(()) => error,
            Err(note) => error.context(format!(
                "the audit note for the changed tasks [{tasks}] was not saved either: {note:#}. \
                 Fix: run `llmenv task session note` to record the override by hand"
            )),
        }
    }
}

/// Refuse a command that acts on session `session_id` (`clear --session`, `session edit`,
/// `session note`, `session finish`) when the caller is not that session.
///
/// The result holds the note when `--other-session` was needed and used. The caller records it
/// once the command has succeeded (#3596).
///
/// # Errors
/// Errors if the caller is another session and `other_session` does not name
/// `session_id`.
pub fn ensure_session_is_ours(
    state_dir: &Path,
    session_id: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<SessionOverride> {
    let own = |text: Option<String>| -> anyhow::Result<SessionOverride> {
        let pending = text
            .as_deref()
            .map(|t| Pending::for_session(state_dir, session_id, t))
            .transpose()?;
        Ok(SessionOverride {
            session_id: session_id.to_string(),
            text,
            pending,
        })
    };
    if matches!(caller, Caller::Session(id) if id == session_id) {
        return own(None);
    }
    match other_session {
        Some(named) if named == session_id => own(Some(format!(
            "acted on with --other-session {session_id}; caller: {}",
            caller.describe()
        ))),
        Some(named) => anyhow::bail!("this command acts on session '{session_id}', not '{named}'"),
        None => ensure_session_is_callers(session_id, caller).and_then(|()| own(None)),
    }
}

/// [`ensure_session_is_ours`] for a read, which takes no `--other-session`, so no note exists.
///
/// # Errors
/// Errors if the caller is not session `session_id`.
pub fn ensure_session_is_callers(session_id: &str, caller: &Caller) -> anyhow::Result<()> {
    if matches!(caller, Caller::Session(id) if id == session_id) {
        return Ok(());
    }
    refuse_unless_caller(
        session_id,
        caller,
        &format!("this command acts on session '{session_id}'"),
    )
}

/// Records that `clear` with `--other-session` deleted `deleted` tasks of session `owner`, as
/// caller `by`. The deletes are saved when this runs (#3481).
///
/// # Errors
/// Errors if the owning session cannot be written. The deletes are already saved, so the error
/// says so.
pub fn record_cleared_override(
    state_dir: &Path,
    owner: &str,
    by: &str,
    deleted: &[String],
) -> anyhow::Result<()> {
    use anyhow::Context;
    let text = format!(
        "Cleared with --other-session by {by}. Deleted tasks: {}.",
        deleted.join(", ")
    );
    let written = session::append_override_note(state_dir, owner, Some(&text)).with_context(|| {
        format!(
            "the tasks were cleared, but the note on session '{owner}' was not saved; \
             do not retry the clear"
        )
    });
    note_not_saved::mark_result(written)
}

/// Refuses a `task add --session <id>` that names a session of another conversation, unless
/// `--other-session` names that session's owner (#3486). A session with no recorded owner, or one
/// that the caller owns, takes the task without a check.
///
/// `sessions` is the read of the session store that the caller made. Returns `true` when the
/// override was used, so the writer can record it.
///
/// # Errors
/// Errors if the session belongs to another conversation and `other_session` does not name its
/// owner.
pub fn ensure_named_session_takes_tasks(
    sessions: &[Session],
    session_id: &str,
    owner: &EngineIdentity,
    other_session: Option<&str>,
) -> anyhow::Result<bool> {
    // A missing or closed session is reported by the add resolver, with its own message.
    let Some(target) = sessions.iter().find(|s| s.id == session_id) else {
        return Ok(false);
    };
    match check_takes_tasks(target, owner) {
        Ok(()) => Ok(false),
        Err(_) if other_session == Some(session_id) => Ok(true),
        Err(refusal) => Err(refusal),
    }
}

/// Refuses a task that would join `session` when the session belongs to another conversation,
/// or when the caller has no conversation id (#3480). The picker of a new task calls this, so
/// a sole open session of another conversation cannot take the task by default.
///
/// # Errors
/// Errors when [`Session::may_take_tasks`] refuses the caller.
pub fn check_takes_tasks(session: &Session, owner: &EngineIdentity) -> anyhow::Result<()> {
    if session.may_take_tasks(owner.session_id.as_deref()) {
        return Ok(());
    }
    anyhow::bail!(foreign_session_message(&session.id))
}

/// Refuses a parent link from a task of `child_session` to a parent in another session (#3480).
/// Starting a child promotes each open ancestor to `wip`, so a cross-session link would change
/// the other session's task. A parent with no session is refused too (#3593).
///
/// # Errors
/// Errors when `parent` belongs to a session other than `child_session`, or when `parent`
/// has no session.
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
    // A session-less parent is refused for every child (#3593). Starting a child promotes its
    // open ancestors, so an owner-less parent would change a task that no session owns.
    match parent.session.as_deref() {
        Some(session) if Some(session) == child_session => Ok(()),
        Some(session) => anyhow::bail!(
            "task '{}' belongs to session '{session}', so it cannot be the parent of a task in \
             another session. Link a task of the same session, or none.",
            parent.slug
        ),
        None => anyhow::bail!(
            "task '{}' has no session, so it cannot be the parent of a task. Fix: link a task \
             of a session, or none.",
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
    anyhow::bail!(
        "{what}, and {}. To act on it anyway, pass the owning session's id as \
         --other-session <SESSION_ID>.",
        caller_reason(caller)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

mod caller;
pub use caller::caller_reason;

/// Refuses the deletion of a task with no session when the caller has no session to record the
/// deletion on. Such a deletion would leave no record of the caller (#3586).
///
/// # Errors
/// Errors if the caller has no open session of its own.
pub fn check_owner_less_delete(caller: &Caller) -> anyhow::Result<()> {
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
pub fn record_owner_less_delete(
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
    let written = session::append_override_note(state_dir, id, Some(&text)).with_context(|| {
        format!(
            "the tasks were deleted, but the note on session '{id}' was not saved; do not retry \
             the delete"
        )
    });
    note_not_saved::mark_result(written)
}
