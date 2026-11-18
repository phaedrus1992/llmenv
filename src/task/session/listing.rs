//! Reads the session store for listings and orders the `session ls` rows.

use std::path::Path;

use anyhow::Context;

#[cfg(test)]
use super::SESSION_READS;
use super::{Session, filter, sessions_dir};

/// Every session in the store, failing on any file that cannot be read or parsed. A corrupt
/// file is an error that names the file, not a skipped entry.
/// The Stop reminder needs the full set, so a missing task in it must come from a real absence
/// and not from a corrupt file (#2628).
///
/// # Errors
/// Returns an error if the store cannot be read or any session file is corrupt.
pub(crate) fn list_sessions_strict(state_dir: &Path) -> anyhow::Result<Vec<Session>> {
    #[cfg(test)]
    SESSION_READS.with(|reads| reads.set(reads.get() + 1));
    let dir = sessions_dir(state_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("reading sessions dir {}", dir.display()))
            );
        }
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry of {}", dir.display()))?
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = crate::paths::bounded::state_text_io(&path)
            .with_context(|| format!("reading session file {}", path.display()))?;
        let session: Session = serde_json::from_str(&content).with_context(|| {
            format!(
                "session file {} is not valid session JSON. Repair or delete the file, then retry",
                path.display()
            )
        })?;
        sessions.push(session);
    }
    Ok(sessions)
}

/// Every open session tagged with `project`, read strictly. A corrupt session file is an error that names the file,
/// so its tasks are not dropped without a word (#2665).
///
/// # Errors
/// Returns an error if the sessions directory cannot be read or a session file is corrupt.
pub(crate) fn open_sessions_strict(
    state_dir: &Path,
    project: &str,
) -> anyhow::Result<Vec<Session>> {
    Ok(filter::open_visible(
        &list_sessions_strict(state_dir)?,
        project,
        None,
    ))
}

/// Orders the `session ls` rows: sessions of `project` first, then by start time.
pub(crate) fn sort_listing(sessions: &mut [Session], project: &str) {
    sessions.sort_by(|a, b| {
        (a.project != project)
            .cmp(&(b.project != project))
            .then_with(|| {
                crate::util::time::cmp_instants(
                    &a.started_at,
                    &a.id,
                    &b.started_at,
                    &b.id,
                    "session started_at",
                )
            })
    });
}
