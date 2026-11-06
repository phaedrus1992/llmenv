//! Chooses the session that a new task joins, from the caller's explicit choice or the open
//! sessions of the project (#3480).

use std::path::Path;

use super::SessionChoice;
use super::ownership;
use super::session;

/// Resolve which session a `task add` belongs to, per the mandatory-sessions
/// rules. An explicit id is honored as long as it names an existing, open
/// session (any project — an explicit id is a deliberate choice). Otherwise
/// it resolves through [`session::pick_open_session`].
///
/// # Errors
/// Errors if the session store cannot be read, if no session is open, if the picked session
/// belongs to another conversation, or if the choice is ambiguous.
pub(super) fn resolve_session_for_add(
    state_dir: &Path,
    session: SessionChoice<'_>,
    project: &str,
) -> anyhow::Result<String> {
    let owner = match session {
        SessionChoice::Named(id) => {
            // Strict `list_sessions_strict`: an unreadable store or a corrupt session
            // file must error out here rather than be misread as "no
            // such session" (#1112, #2665) — this is the same resolution step `TaskCreate`
            // (via `add_task`) runs through, so the hook redirect's own unreadable-
            // store guard in `task_tools::create` would otherwise be undone here.
            let exists_open = session::list_sessions_strict(state_dir)?
                .into_iter()
                .any(|s| s.id == id && s.is_open());
            if !exists_open {
                anyhow::bail!("session '{id}' does not exist or is not open");
            }
            return Ok(id.to_string());
        }
        SessionChoice::Resolve(owner) => owner,
        SessionChoice::Checked(session) => {
            if !session.is_open() || session.project != project {
                anyhow::bail!(
                    "session '{}' is not an open session of this project",
                    session.id
                );
            }
            return Ok(session.id.clone());
        }
    };
    let open = session::open_sessions_strict(state_dir, project)?;
    match session::pick_open_session(open, owner) {
        Ok(session) => {
            ownership::check_takes_tasks(&session, owner)?;
            Ok(session.id)
        }
        Err(session::PickError::NoneOpen) => anyhow::bail!(
            "no open session for this project — run `llmenv task session start` first, \
             or pass --session <id>"
        ),
        Err(e) => anyhow::bail!(
            "{}",
            e.ambiguity_message("pass --session <id>, or see `llmenv task session ls`")
                .unwrap_or_default()
        ),
    }
}
