//! Picks the session a `llmenv task session` command acts on when no id is named.

use crate::cli::task_guard::guard_session;

/// Resolve an explicit-or-omitted session id the same way `add_task` does:
/// omitted resolves through [`crate::task::session::pick_open_session`], so
/// with two or more open the caller's own session wins (#2365).
pub(super) fn resolve_session_id(
    state_dir: &std::path::Path,
    project: &str,
    id: Option<String>,
) -> anyhow::Result<String> {
    use crate::task::session::{self, EngineIdentity, PickError};
    if let Some(id) = id {
        return Ok(id);
    }
    // Fallible read: an unreadable store must not read as "no open session" (#1112).
    let open = session::open_sessions_strict(state_dir, project)?;
    match session::pick_open_session(open, &EngineIdentity::from_env()?) {
        Ok(session) => Ok(session.id),
        Err(PickError::NoneOpen) => {
            anyhow::bail!("no open session for this project — pass an id explicitly")
        }
        Err(e) => anyhow::bail!(
            "{}",
            e.ambiguity_message("pass an id explicitly")
                .unwrap_or_else(|| "more than one session is open".to_string())
        ),
    }
}

/// [`resolve_session_id`] for a read (`session show`, `session summary`). An omitted id
/// must not pick another conversation's session, so the pick passes the ownership guard.
/// An explicit id is read as given, because the caller named it (#3588).
pub(super) fn resolve_session_for_read(
    state_dir: &std::path::Path,
    project: &str,
    id: Option<String>,
) -> anyhow::Result<String> {
    if let Some(id) = id {
        return Ok(id);
    }
    let picked = resolve_session_id(state_dir, project, None)?;
    guard_session(state_dir, &picked, None)?;
    Ok(picked)
}
