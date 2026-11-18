//! Abandoning a session for `session start --replace`, and the error when that fails.

use std::path::Path;

use super::{Session, save_session};
use crate::task::partial::PartialWrite;
use crate::task::session::finish::untag_unfinished;
use crate::task::{Task, now_rfc3339};

/// The error of a failed abandon in `session start --replace`. It names the sessions that were
/// abandoned before it, and the tasks that the failed session had already untagged, because no new
/// session is started and the user must know what changed (#3759).
pub(super) fn abandon_failed(
    id: &str,
    abandoned: &[Session],
    error: anyhow::Error,
) -> anyhow::Error {
    let sessions: Vec<&str> = abandoned.iter().map(|s| s.id.as_str()).collect();
    let untagged: Vec<String> = crate::task::partial::changed_before_failure(&error)
        .iter()
        .map(|t| t.slug.clone())
        .collect();
    // An error with no list may still follow an untag, so an empty list reads as "none known".
    let untagged = if untagged.is_empty() {
        "none known".to_owned()
    } else {
        untagged.join(", ")
    };
    error.context(format!(
        "cannot abandon session '{id}', and no new session is started. Already abandoned \
         sessions: [{}]. Tasks already untagged from '{id}': [{untagged}]. Fix: run `llmenv task \
         session start --replace` again once the store is writable",
        sessions.join(", "),
    ))
}

/// Abandon `session`: stamps `abandoned_at`, and for every one of its tasks
/// that isn't already `done`, clears the `session` tag and appends an
/// orphaning note. Already-`done` tasks keep their tag — a legitimate
/// historical record. `tasks` are the tasks of `session`, from the caller's strict read. Caller
/// must already hold the store lock. Returns the
/// stamped session, so the caller doesn't re-read what was just written.
pub(super) fn abandon_session(
    state_dir: &Path,
    mut session: Session,
    tasks: Vec<Task>,
) -> anyhow::Result<Session> {
    let now = now_rfc3339();
    let label = session.name.clone().unwrap_or_else(|| session.id.clone());
    let note = format!(
        "Orphaned: session '{label}' was abandoned (`session start --replace`) \
         before this task was finished."
    );
    // The stamp comes last, so a failed untag leaves the session open and a retry finds it.
    let untagged = untag_unfinished(state_dir, &session.id, tasks, &now, &note)?;
    session.abandoned_at = Some(now);
    // The tasks are untagged when this save fails, so the error carries them (#3759).
    save_session(state_dir, &session).map_err(|e| PartialWrite::error(untagged, e))?;
    Ok(session)
}
