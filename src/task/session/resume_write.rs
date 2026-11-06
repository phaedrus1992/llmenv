//! Writes to the resume context of a stored session.

use std::path::Path;

use anyhow::Context;

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

/// Appends an `--other-session` audit note to session `id`, open or closed. Nothing is written
/// when `note` is `None`. The caller runs this after its change has saved, under the same store
/// lock, so a closed session is allowed here: `session finish` closes the session first (#3596).
///
/// # Errors
/// Errors if the session cannot be read or written. The change itself is already saved.
pub(crate) fn append_override_note(
    state_dir: &Path,
    id: &str,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let Some(text) = note else {
        return Ok(());
    };
    with_store_lock(state_dir, || {
        let mut session = load_existing_session(state_dir, id)?;
        session.resume.append_note(text);
        save_session(state_dir, &session)
    })
    .with_context(|| {
        format!("the change to session '{id}' is saved, but its override note was not written")
    })
}
