//! Refuses `task start`, `done`, and `wait` on a task that belongs to another
//! session (#2584). A bare `--force` does not override this check; only the
//! explicit `--other-session` flag does.

use std::path::Path;

use super::resolve::resolve_identifier;
use super::session::{self, EngineIdentity};

/// The open session this conversation works in, for `project`.
///
/// Returns `None` when no open session is owned by the caller, or when more than
/// one is open and none is the caller's. The store read itself still fails loudly.
///
/// # Errors
/// Errors if the session store cannot be read.
pub(crate) fn caller_session(
    state_dir: &Path,
    project: &str,
    owner: &EngineIdentity,
) -> anyhow::Result<Option<String>> {
    let open = session::try_open_sessions_for_project(state_dir, project)?;
    Ok(session::pick_open_session(open, owner)
        .ok()
        .map(|session| session.id))
}

/// Refuse to act on the task `input` when it belongs to a session other than
/// `caller`. A task with no session is never refused. `other_session` lifts the
/// refusal for a deliberate cross-session action.
///
/// # Errors
/// Errors if `input` does not resolve to a task, the store cannot be read, or the
/// task belongs to another session and `other_session` is `false`.
pub(crate) fn ensure_task_is_ours(
    state_dir: &Path,
    input: &str,
    caller: Option<&str>,
    other_session: bool,
) -> anyhow::Result<()> {
    if other_session {
        return Ok(());
    }
    let slug = resolve_identifier(state_dir, input)?;
    let task = super::load_task(state_dir, &slug)?;
    let Some(owner) = task.session.as_deref() else {
        return Ok(());
    };
    if caller == Some(owner) {
        return Ok(());
    }
    let who = caller.map_or_else(
        || "any open session of yours".to_string(),
        |id| format!("your session '{id}'"),
    );
    anyhow::bail!(
        "task '{slug}' belongs to session '{owner}', not {who}. Pass --other-session to act \
         on it anyway."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::session::{StartDecision, StartOutcome, start_session};
    use super::super::{ParentSpec, SessionChoice, TaskState, add_task, load_task, save_task};
    use super::ensure_task_is_ours;
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

    #[test]
    fn other_sessions_task_is_refused_and_left_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());

        let err = ensure_task_is_ours(dir.path(), &slug, Some("some-other-session"), false)
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
    fn caller_with_no_session_is_refused_for_a_session_task() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, _owner) = open_task_in_session(dir.path());

        assert!(ensure_task_is_ours(dir.path(), &slug, None, false).is_err());
    }

    #[test]
    fn owning_session_may_act_on_its_own_task() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, owner) = open_task_in_session(dir.path());

        ensure_task_is_ours(dir.path(), &slug, Some(&owner), false)
            .expect("the owning session must pass");
    }

    #[test]
    fn other_session_flag_lifts_the_refusal() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, _owner) = open_task_in_session(dir.path());

        ensure_task_is_ours(dir.path(), &slug, Some("some-other-session"), true)
            .expect("--other-session must allow the action");
    }

    #[test]
    fn task_without_a_session_is_never_refused() {
        let dir = TempDir::new().expect("tempdir");
        let (slug, _owner) = open_task_in_session(dir.path());
        // Clear the tag directly: a task added outside any session has no owner.
        let mut task = load_task(dir.path(), &slug).expect("load");
        task.session = None;
        save_task(dir.path(), &task).expect("save");

        ensure_task_is_ours(dir.path(), &slug, Some("some-other-session"), false)
            .expect("a session-less task must not be refused");
    }
}
