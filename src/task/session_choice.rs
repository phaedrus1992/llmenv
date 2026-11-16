//! Chooses the session that a new task joins, from the caller's explicit choice or the open
//! sessions of the project (#3480).

use std::path::Path;

use super::SessionChoice;
use super::ownership;
use super::session;

/// The session that a new task joins, and whether `--other-session` was needed for it.
#[derive(Debug)]
pub(super) struct ResolvedSession {
    pub(super) id: String,
    /// True when the caller named the owner of another conversation's session. The writer records
    /// a note on the new task (#3486).
    pub(super) overridden: bool,
}

/// Resolve which session a `task add` belongs to, per the mandatory-sessions rules, and refuse a
/// session of another conversation for every choice (#3617). An explicit id is honored as long as
/// it names an existing, open session (any project: an explicit id is a deliberate choice).
/// Otherwise it resolves through [`session::pick_open_session`].
///
/// # Errors
/// Errors if the session store cannot be read, if no session is open, if the chosen session
/// belongs to another conversation, or if the choice is ambiguous.
pub(super) fn resolve_session_for_add(
    state_dir: &Path,
    session: SessionChoice<'_>,
    project: &str,
) -> anyhow::Result<ResolvedSession> {
    let owner = match session {
        SessionChoice::Named {
            id,
            owner,
            other_session,
        } => return resolve_named(state_dir, id, owner, other_session),
        SessionChoice::Resolve(owner) => owner,
        SessionChoice::Checked { session, owner } => {
            // The caller's copy may be older than the file. One session file is read again, under
            // the lock, so a session finished in between is refused (#3474). That reads one file,
            // not the whole session store.
            let fresh = session::load_existing_session(state_dir, &session.id)?;
            if !fresh.is_open() || fresh.project != project {
                anyhow::bail!(
                    "session '{}' is not an open session of this project. Fix: run `llmenv task \
                     session ls` to list the open sessions.",
                    session.id
                );
            }
            ownership::check_takes_tasks(&fresh, owner)?;
            return Ok(ResolvedSession {
                id: fresh.id,
                overridden: false,
            });
        }
    };
    let open = session::open_sessions_strict(state_dir, project)?;
    match session::pick_open_session(open, owner) {
        Ok(session) => {
            ownership::check_takes_tasks(&session, owner)?;
            Ok(ResolvedSession {
                id: session.id,
                overridden: false,
            })
        }
        Err(session::PickError::NoneOpen) => anyhow::bail!(
            "no open session for this project — run `llmenv task session start` first, \
             or pass --session <id>"
        ),
        Err(e) => anyhow::bail!(
            "{}",
            e.message("pass --session <id>, or see `llmenv task session ls`")
        ),
    }
}

/// The session that `--session <id>` names, from one read of the session store.
fn resolve_named(
    state_dir: &Path,
    id: &str,
    owner: &session::EngineIdentity,
    other_session: Option<&str>,
) -> anyhow::Result<ResolvedSession> {
    // Strict `list_sessions_strict`: an unreadable store or a corrupt session file must error out
    // here rather than be misread as "no such session" (#1112, #2665).
    let sessions = session::list_sessions_strict(state_dir)?;
    let overridden =
        ownership::ensure_named_session_takes_tasks(&sessions, id, owner, other_session)?;
    if !sessions.iter().any(|s| s.id == id && s.is_open()) {
        anyhow::bail!(
            "session '{id}' does not exist or is not open. Fix: run `llmenv task session ls` \
             to list the open sessions, then pass one of their ids."
        );
    }
    Ok(ResolvedSession {
        id: id.to_string(),
        overridden,
    })
}
