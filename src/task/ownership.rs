//! Refuses task commands that act on another session's tasks (#2584). A bare
//! `--force` does not override this check. Only `--other-session <owner>` does,
//! and only when `<owner>` is the session that owns the task (#2591).

use std::path::Path;

use super::resolve::resolve_identifier;
use super::session::{self, EngineIdentity};

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
    fn describe(&self) -> String {
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
    let open = session::try_open_sessions_for_project(state_dir, project)?;
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
pub(crate) struct OverrideNote {
    slug: String,
    text: String,
}

impl OverrideNote {
    /// Appends the note to the task.
    ///
    /// # Errors
    /// Errors if the task cannot be read or written.
    pub(crate) fn record(&self, state_dir: &Path) -> anyhow::Result<()> {
        super::note_task(state_dir, &self.slug, &self.text)?;
        Ok(())
    }
}

/// Refuse to act on the task `input` when it belongs to a session other than the
/// caller's. A task with no session is never refused.
///
/// `other_session` is the owning session id named by `--other-session`. The
/// override applies only when that id is the task's owner. It returns the note
/// that records the override. This function writes nothing itself.
///
/// # Errors
/// Errors if `input` does not resolve to a task, the store cannot be read, the
/// task belongs to another session and no matching `other_session` is given, or
/// the named `other_session` is not the owner.
pub(crate) fn ensure_task_is_ours(
    state_dir: &Path,
    input: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<Option<OverrideNote>> {
    let slug = resolve_identifier(state_dir, input)?;
    let task = super::load_task(state_dir, &slug)?;
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

/// Refuse `clear --session <owner>` when the caller is not that session.
///
/// # Errors
/// Errors if the caller is another session and `other_session` does not name
/// `session_id`.
pub(crate) fn ensure_session_is_ours(
    session_id: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    if matches!(caller, Caller::Session(id) if id == session_id) {
        return Ok(());
    }
    match other_session {
        Some(named) if named == session_id => Ok(()),
        Some(named) => anyhow::bail!(
            "'--session {session_id}' clears the tasks of session '{session_id}', not '{named}'"
        ),
        None => refuse_unless_caller(
            session_id,
            caller,
            &format!("'--session {session_id}' clears the tasks of session '{session_id}'"),
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
mod tests {
    use super::super::resume::ResumeContext;
    use super::super::session::{EngineIdentity, StartDecision, StartOutcome, start_session};
    use super::super::session::{StartRequest, start_session_as};
    use super::super::{ParentSpec, SessionChoice, TaskState, add_task, load_task, save_task};
    use super::{Caller, caller_session, ensure_session_is_ours, ensure_task_is_ours};
    use tempfile::TempDir;

    const PROJECT: &str = "ownership-test";

    fn open_task_in_session(dir: &std::path::Path) -> (String, String) {
        let outcome = start_session(dir, Some("a"), None, PROJECT, StartDecision::Auto)
            .expect("start session");
        let StartOutcome::Created(session) = outcome else {
            panic!("expected a new session");
        };
        let task = add_task(
            dir,
            "Owned step",
            ParentSpec::Detached,
            SessionChoice::Named(&session.id),
            PROJECT,
        )
        .expect("add task");
        (task.slug, session.id)
    }

    fn conversation(id: &str) -> EngineIdentity {
        EngineIdentity {
            session_id: Some(id.to_string()),
            pid: None,
        }
    }

    #[test]
    fn other_sessions_task_is_refused_and_left_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());
        let caller = Caller::Session("some-other-session".to_string());

        let err = ensure_task_is_ours(dir.path(), &slug, &caller, None)
            .expect_err("a task of another session must be refused");

        let msg = format!("{err:#}");
        assert!(
            msg.contains(&owner),
            "refusal must name the owning session: {msg}"
        );
        assert!(
            msg.contains("--other-session"),
            "refusal must name the flag: {msg}"
        );
        let task = load_task(dir.path(), &slug).expect("load");
        assert_eq!(
            task.state,
            TaskState::Open,
            "a refused task must not change"
        );
    }

    #[test]
    fn unidentified_caller_is_refused_and_the_reason_is_named() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, _owner) = open_task_in_session(dir.path());
        let caller = Caller::Unidentified("no session of yours is open in this project".into());

        let err = ensure_task_is_ours(dir.path(), &slug, &caller, None)
            .expect_err("an unidentified caller must not act on a session task");

        assert!(
            format!("{err:#}").contains("no session of yours"),
            "{err:#}"
        );
    }

    #[test]
    fn owning_session_may_act_on_its_own_task() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());

        ensure_task_is_ours(dir.path(), &slug, &Caller::Session(owner), None)
            .expect("the owning session must pass");
    }

    #[test]
    fn other_session_naming_the_owner_lifts_the_refusal() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());
        let caller = Caller::Unidentified("no session".into());

        ensure_task_is_ours(dir.path(), &slug, &caller, Some(&owner))
            .expect("--other-session with the owner's id must allow the action");
    }

    #[test]
    fn other_session_naming_another_session_is_refused() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());
        let caller = Caller::Session("some-other-session".to_string());

        let err = ensure_task_is_ours(dir.path(), &slug, &caller, Some("wrong-owner"))
            .expect_err("a non-owner id must not lift the refusal");

        let msg = format!("{err:#}");
        assert!(msg.contains(&owner), "must name the real owner: {msg}");
        assert!(msg.contains("wrong-owner"), "must name the given id: {msg}");
        let task = load_task(dir.path(), &slug).expect("load");
        assert!(
            task.notes.is_empty(),
            "a refused override must leave no note"
        );
    }

    #[test]
    fn other_session_override_notes_the_caller_only_after_record() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());
        let caller = Caller::Session("some-other-session".to_string());

        let pending = ensure_task_is_ours(dir.path(), &slug, &caller, Some(&owner))
            .expect("the named owner must allow the action")
            .expect("an override returns the note to record");
        assert!(
            load_task(dir.path(), &slug).expect("load").notes.is_empty(),
            "the guard alone must not write a note"
        );

        pending.record(dir.path()).expect("record");

        let task = load_task(dir.path(), &slug).expect("load");
        let noted = task
            .notes
            .iter()
            .any(|n| n.text.contains("--other-session") && n.text.contains("some-other-session"));
        assert!(noted, "the override must note the caller: {:?}", task.notes);
    }

    #[test]
    fn task_without_a_session_is_never_refused() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, _owner) = open_task_in_session(dir.path());
        // Clear the tag directly: a task added outside any session has no owner.
        let mut task = load_task(dir.path(), &slug).expect("load");
        task.session = None;
        save_task(dir.path(), &task).expect("save");

        ensure_task_is_ours(
            dir.path(),
            &slug,
            &Caller::Unidentified("none".into()),
            None,
        )
        .expect("a session-less task must not be refused");
    }

    #[test]
    fn session_owned_by_another_conversation_is_not_the_callers() {
        let dir = TempDir::new().expect("tempdir");
        let project_owner = conversation("conv-a");
        let request = StartRequest {
            name: Some("a"),
            description: None,
            project: PROJECT,
            owner: &project_owner,
            resume: &ResumeContext::default(),
        };
        start_session_as(dir.path(), &request, StartDecision::Auto).expect("start");

        let caller = caller_session(dir.path(), PROJECT, &conversation("conv-b")).expect("caller");

        assert!(
            matches!(&caller, Caller::Unidentified(reason) if reason.contains("another conversation")),
            "conv-b must not inherit conv-a's only session: {caller:?}"
        );
    }

    #[test]
    fn several_open_sessions_and_no_identity_is_unidentified() {
        let dir = TempDir::new().expect("tempdir");
        start_session(dir.path(), Some("one"), None, PROJECT, StartDecision::Auto).expect("one");
        start_session(dir.path(), Some("two"), None, PROJECT, StartDecision::New).expect("two");

        let caller = caller_session(dir.path(), PROJECT, &EngineIdentity::default()).expect("ok");

        assert!(matches!(caller, Caller::Unidentified(_)), "{caller:?}");
    }

    #[test]
    fn clearing_another_sessions_tasks_is_refused() {
        let caller = Caller::Session("mine".to_string());

        let err = ensure_session_is_ours("theirs", &caller, None)
            .expect_err("clear --session of another session must be refused");

        assert!(format!("{err:#}").contains("theirs"), "{err:#}");
        assert!(ensure_session_is_ours("mine", &caller, None).is_ok());
    }

    #[test]
    fn clearing_another_session_needs_its_own_id_as_the_flag_value() {
        let caller = Caller::Session("mine".to_string());

        assert!(ensure_session_is_ours("theirs", &caller, Some("theirs")).is_ok());
        assert!(ensure_session_is_ours("theirs", &caller, Some("mine")).is_err());
    }
}
