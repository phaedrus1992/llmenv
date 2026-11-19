//! Reads the session store for listings and orders the `session ls` rows.

use std::path::Path;

use anyhow::Context;

#[cfg(test)]
use super::SESSION_READS;
use super::{Session, filter, sessions_dir};
use crate::util::time::RecordId;

/// Parse the JSON of one session file, then check its `started_at`. `sort_listing` orders on that
/// field, so a value that does not parse would sort first with no cue (#3882). Every strict read
/// of a session file uses this, so one file gets one verdict.
///
/// # Errors
/// Errors if `content` is not session JSON, or the `started_at` is not RFC 3339. The message names
/// the file and the fix.
pub(super) fn parse_session(content: &str, path: &Path) -> anyhow::Result<Session> {
    let session: Session = serde_json::from_str(content).with_context(|| {
        format!(
            "session file {} is not valid session JSON. Repair or delete the file, then retry",
            path.display()
        )
    })?;
    let record = format!("session '{}' at {}", session.id, path.display());
    crate::util::time::validate_stored_field("started_at", &session.started_at, &record)?;
    Ok(session)
}

/// Every session in the store, failing on any file that cannot be read or parsed, or that has a
/// `started_at` that is not RFC 3339. A corrupt file is an error that names the file, not a
/// skipped entry.
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
            tracing::debug!(path = %path.display(), "skipped a store entry that is not a .json file");
            continue;
        }
        let content = crate::paths::bounded::state_text_io(&path)
            .with_context(|| format!("reading session file {}", path.display()))?;
        let session = parse_session(&content, &path)?;
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

/// Orders the `session ls` rows: sessions of `project` first, then by start time, then by id.
/// Two sessions with the same start time are ordered by id, so the order does not depend on the
/// order that `read_dir` returns the files in. The strict read refuses an unreadable start time.
pub(crate) fn sort_listing(sessions: &mut [Session], project: &str) {
    sessions.sort_by(|a, b| {
        (a.project != project)
            .cmp(&(b.project != project))
            .then_with(|| {
                crate::util::time::cmp_instants(
                    &a.started_at,
                    RecordId(&a.id),
                    &b.started_at,
                    RecordId(&b.id),
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
    use super::{Session, list_sessions_strict, sessions_dir, sort_listing};
    use proptest::prelude::*;
    use tempfile::TempDir;

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

    #[test]
    fn the_strict_read_fails_on_an_unreadable_started_at_and_names_the_fix() {
        let dir = TempDir::new().expect("test");
        std::fs::create_dir_all(sessions_dir(dir.path())).expect("test");
        let path = sessions_dir(dir.path()).join("bad.json");
        let bad = serde_json::to_string(&session("bad", "p", "yesterday")).expect("serialize");
        std::fs::write(&path, bad).expect("test");

        let err = list_sessions_strict(dir.path()).expect_err("a bad started_at must fail");
        let text = format!("{err:#}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("unreadable started_at"), "{text}");
        assert!(text.contains("Fix: rewrite it as RFC 3339"), "{text}");
    }

    /// Code 0 is an unreadable start time. Codes 1 to 3 are readable instants.
    fn started_at(code: u8) -> String {
        match code {
            0 => "garbage".to_string(),
            n => format!("2026-01-01T00:00:0{n}Z"),
        }
    }

    proptest! {
        // The strict read fails exactly when `started_at` is not RFC 3339 (#3882).
        #[test]
        fn the_strict_read_agrees_with_the_timestamp_validator(
            started_at in prop_oneof![
                (0i64..4_000_000_000).prop_map(|s| jiff::Timestamp::from_second(s)
                    .expect("in range")
                    .to_string()),
                "\\PC{0,30}",
            ],
        ) {
            let dir = TempDir::new().expect("test");
            std::fs::create_dir_all(sessions_dir(dir.path())).expect("test");
            let body = serde_json::to_string(&session("p1", "p", &started_at)).expect("serialize");
            std::fs::write(sessions_dir(dir.path()).join("p1.json"), body).expect("test");
            let valid = crate::util::time::validate_stored(&started_at).is_ok();
            prop_assert_eq!(list_sessions_strict(dir.path()).is_ok(), valid);
        }

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
