//! Writes to the resume context of a stored session.

use std::path::Path;

use crate::task::resume::ResumeContext;
use crate::task::{now_rfc3339, with_store_lock};

use super::{Session, load_existing_session, save_session};

/// Change the resume context of an open session and bump its activity time.
///
/// # Errors
/// The session does not exist, is closed, or cannot be saved.
pub(crate) fn update_resume(
    state_dir: &Path,
    id: &str,
    change: impl FnOnce(&mut ResumeContext),
) -> anyhow::Result<Session> {
    with_store_lock(state_dir, || {
        let mut session = load_existing_session(state_dir, id)?;
        if !session.is_open() {
            anyhow::bail!("session '{id}' is closed and cannot be changed");
        }
        change(&mut session.resume);
        session.last_activity = now_rfc3339();
        save_session(state_dir, &session)?;
        Ok(session)
    })
}

/// Appends an `--other-session` audit note to session `id`, open or closed. The caller runs it
/// after its change has saved, so a failed command leaves no note. A closed session is allowed
/// here, because `session finish` closes the session before it writes the note (#3596).
///
/// # Errors
/// Errors if the session cannot be read or written.
pub(crate) fn append_override_note(state_dir: &Path, id: &str, text: &str) -> anyhow::Result<()> {
    with_store_lock(state_dir, || {
        let mut session = load_existing_session(state_dir, id)?;
        session.resume.append_note(text);
        save_session(state_dir, &session)
    })
}
