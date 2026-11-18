//! Reads the session store for listings and orders the `session ls` rows.

use std::path::Path;

use anyhow::Context;

use super::SESSION_READS;
use super::{Session, filter, sessions_dir};

/// Every session in the store, failing on any file that cannot be read or parsed. A corrupt
/// file is an error that names the file, not a skipped entry.
/// The Stop reminder needs the full set, so a missing task in it must come from a real absence
/// and not from a corrupt file (#2628).
///
/// # Errors
/// Returns an error if the store cannot be read or any session file is corrupt.
pub fn list_sessions_strict(state_dir: &Path) -> anyhow::Result<Vec<Session>> {
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
            tracing::debug!(path = %path.display(), "skipped a store entry that is not a .json file");
            continue;
        }
        let content = super::read_state_text(&path)
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
pub fn open_sessions_strict(state_dir: &Path, project: &str) -> anyhow::Result<Vec<Session>> {
    Ok(filter::open_visible(
        &list_sessions_strict(state_dir)?,
        project,
        None,
    ))
}

/// Orders the `session ls` rows: sessions of `project` first, then by start time, then by id.
/// The id breaks the tie that an unreadable start time leaves, so the order does not depend on
/// the order that `read_dir` returns the files in.
pub fn sort_listing(sessions: &mut [Session], project: &str) {
    sessions.sort_by(|a, b| {
        (a.project != project)
            .cmp(&(b.project != project))
            .then_with(|| {
                llmenv_util::time::cmp_instants(
                    &a.started_at,
                    &a.id,
                    &b.started_at,
                    &b.id,
                    "session started_at",
                )
                .then_with(|| a.id.cmp(&b.id))
            })
    });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use super::{Session, sort_listing};
    use proptest::prelude::*;

    fn session(id: &str, project: &str, started_at: &str) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": null,
            "project": project,
            "started_at": started_at,
            "last_activity": started_at,
        }))
        .expect("a minimal session parses")
    }

    /// Code 0 is an unreadable start time. Codes 1 to 3 are readable instants.
    fn started_at(code: u8) -> String {
        match code {
            0 => "garbage".to_string(),
            n => format!("2026-01-01T00:00:0{n}Z"),
        }
    }

    proptest! {
        // The listing must not depend on the order of the input. Unreadable start times compare
        // equal, so only the id keeps the order fixed.
        #[test]
        fn sort_listing_does_not_depend_on_input_order(
            rows in proptest::collection::vec(("[a-c]{1,2}", "[pq]", 0u8..4), 0..10),
        ) {
            let mut forward: Vec<Session> = rows
                .iter()
                .map(|(id, project, code)| session(id, project, &started_at(*code)))
                .collect();
            let mut backward = forward.clone();
            backward.reverse();
            sort_listing(&mut forward, "p");
            sort_listing(&mut backward, "p");
            let ids = |s: &[Session]| s.iter().map(|x| x.id.clone()).collect::<Vec<_>>();
            prop_assert_eq!(ids(&forward), ids(&backward));
        }
    }
}
