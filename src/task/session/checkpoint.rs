//! The `session start` checkpoint: which open sessions the caller owns, the resume of one, and the
//! refusal that lists them.

use std::path::Path;
use std::time::SystemTime;

use super::{EngineIdentity, Session, StartRequest, now_rfc3339, save_session};

mod engine_start;
mod ownership;
pub(super) use engine_start::engine_start;
use ownership::{Ownership, is_callers, ownership, ownership_note, unreadable_last_activity};

/// The one listed session that `owner` started, or `None` when it started none or more than one.
/// After a `/clear` or a compaction the choice is then clear, so `Auto` resumes it (#2904).
///
/// A process id is reused after its process exits, so a session that only the process id ties to
/// the caller counts only when it was active after the caller's engine started (`engine_start`).
/// Without that time, only a session of the same conversation counts.
pub(super) fn only_own_session<'a>(
    mut existing: impl Iterator<Item = &'a Session>,
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
) -> Option<&'a Session> {
    let mut own = existing
        .by_ref()
        .filter(|s| is_callers(s, owner, engine_start));
    match (own.next(), own.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Build the `session start` checkpoint error message: lists every existing
/// open same-project session with enough detail (id, name, description,
/// idle duration, and whether `owner` started it) that the agent or a human
/// can decide `--resume`, `--replace`, or `--new` without needing to inspect
/// anything further.
pub(super) fn checkpoint_error(
    existing: &[Session],
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
) -> String {
    let lines: Vec<String> = existing
        .iter()
        .map(|s| {
            let idle = idle_display(&s.last_activity, &s.id);
            format!(
                "  - {} ({}){} — idle {idle}{}",
                s.id,
                s.name.as_deref().unwrap_or("unnamed"),
                s.description
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default(),
                ownership_note(s, owner, engine_start),
            )
        })
        .collect();
    let yours = existing
        .iter()
        .filter(|s| {
            matches!(
                ownership(s, owner, engine_start),
                Ownership::Conversation | Ownership::Process(_)
            )
        })
        .count();
    let unknown = existing
        .iter()
        .filter(|s| matches!(ownership(s, owner, engine_start), Ownership::Unknown))
        .count();
    // `--replace` abandons every listed session, so it is only offered as the
    // way to drop "yours" when every listed session is yours.
    let mut hint = if yours == 0 {
        String::new()
    } else if yours == existing.len() {
        "\nEvery listed session is yours: pass --resume <id> to continue one, or --replace \
         to drop them all. Use --new only for a second, parallel window."
            .to_string()
    } else {
        format!(
            "\nA session marked as yours holds your own earlier work: pass --resume <id> to \
             continue it, or close it with `llmenv task session finish <id>`. --replace \
             abandons all {} listed sessions, including the ones that are not yours.",
            existing.len()
        )
    };
    if unknown > 0 {
        hint.push_str(
            "\nllmenv could not read when this engine process started, so it cannot tell which \
             sessions marked \"may be yours\" are yours. Fix: check that /bin/ps works, then run \
             the command again, or pass --resume <id>.",
        );
    }
    format!(
        "session(s) already open for this project:\n{}\n\
         pass one of --resume <id>, --replace, or --new{hint}",
        lines.join("\n")
    )
}

pub(super) fn resume(
    state_dir: &Path,
    mut session: Session,
    request: &StartRequest<'_>,
) -> anyhow::Result<Session> {
    session.last_activity = now_rfc3339();
    session.claim(request.owner);
    session.resume.apply(request.resume);
    save_session(state_dir, &session)?;
    Ok(session)
}

/// Human-readable idle duration since an RFC3339 `last_activity` timestamp
/// (e.g. `"2h 5m 3s"`), for `session ls` and the `session start` checkpoint.
/// `"unknown"` with the reason when the timestamp can't be parsed or is in the future. The
/// reason shows in the listing, because a notice reaches only hook output (#3859).
#[must_use]
pub(crate) fn idle_display(last_activity: &str, session_id: &str) -> String {
    let now = std::time::SystemTime::now();
    let Some(at) = crate::util::time::instant_or_log(
        last_activity,
        "session last_activity",
        crate::util::time::RecordId(session_id),
    ) else {
        // The listing has no other place for the cause, so the notice carries it (#3859).
        crate::agent_notice::record_logged(unreadable_last_activity(session_id));
        return "unknown: last_activity is unreadable".to_string();
    };
    match now.duration_since(at) {
        Ok(d) => crate::util::time::format_age(std::time::Duration::from_secs(d.as_secs())),
        Err(_) => {
            // Each listing row calls this. A repeat of the notice is not logged again (#3811).
            crate::agent_notice::record_logged(format!(
                "session {session_id} has a last_activity in the future, so its idle time is \
                 unknown. Fix: check the clock of the machine that wrote it"
            ));
            "unknown: last_activity is in the future".to_string()
        }
    }
}

/// Refuse `--replace` when it would abandon an owned session of another conversation that the
/// caller did not name with `--other-session`. A session with no owner has no one to protect, so
/// it is abandoned as before (#3487).
///
/// # Errors
/// Errors if `named` holds an id that is not an open session of the project, or if a foreign
/// session is left unnamed. The message lists each such session and the flag that allows it.
pub(super) fn refuse_unnamed_foreign(
    existing: &[Session],
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
    named: &[String],
) -> anyhow::Result<()> {
    if let Some(unknown) = named.iter().find(|n| !existing.iter().any(|s| &s.id == *n)) {
        anyhow::bail!(
            "--other-session names '{unknown}', which is not an open session of this project"
        );
    }
    let unnamed: Vec<&Session> = existing
        .iter()
        .filter(|s| s.owner_session.is_some() || s.owner_pid.is_some())
        .filter(|s| !is_callers(s, owner, engine_start))
        .filter(|s| !named.contains(&s.id))
        .collect();
    if unnamed.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = unnamed
        .iter()
        .map(|s| match ownership(s, owner, engine_start) {
            Ownership::Unknown => format!("{} (cannot tell if it is yours)", s.id),
            _ => s.id.clone(),
        })
        .collect();
    let flags: String = unnamed
        .iter()
        .map(|s| format!(" --other-session {}", s.id))
        .collect();
    let cannot_tell = if unnamed
        .iter()
        .any(|s| matches!(ownership(s, owner, engine_start), Ownership::Unknown))
    {
        "\nllmenv could not read when this engine process started, so it cannot tell whether a \
         session marked \"cannot tell\" is yours. Fix: check that /bin/ps works, then run again."
    } else {
        ""
    };
    anyhow::bail!(
        "--replace would abandon session(s) of another conversation: {}{cannot_tell}\n\
         Nothing was changed. To abandon them anyway, run again with{flags}",
        listed.join(", ")
    )
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn idle_display_names_an_unreadable_last_activity_in_the_notice() {
        assert_eq!(
            idle_display("yesterday", "s10"),
            "unknown: last_activity is unreadable"
        );
        crate::agent_notice::assert_noticed(&["unreadable last_activity", "s10"]);
    }

    #[test]
    fn idle_display_names_a_future_last_activity_in_the_notice() {
        assert_eq!(
            idle_display("2999-01-01T00:00:00Z", "s11"),
            "unknown: last_activity is in the future"
        );
        crate::agent_notice::assert_noticed(&["last_activity in the future", "s11"]);
    }

    #[test]
    fn a_future_last_activity_has_no_idle_time() {
        assert_eq!(
            idle_display("9999-01-01T00:00:00Z", "s1"),
            "unknown: last_activity is in the future"
        );
    }

    proptest! {
        // Arbitrary stored text never panics. A time shows only for text that parses.
        #[test]
        fn idle_display_shows_a_time_only_for_text_that_parses(text in "\\PC{0,40}") {
            let shown = idle_display(&text, "s1");
            if !shown.starts_with("unknown") {
                prop_assert!(crate::util::time::instant_or_log(&text, "test", crate::util::time::RecordId("s1")).is_some());
            }
        }
    }

    fn session(id: usize, conversation: Option<u8>, pid: Option<u32>) -> Session {
        Session {
            id: format!("s{id}"),
            name: None,
            project: "p".into(),
            description: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            last_activity: "2026-01-01T00:00:00Z".into(),
            finished_at: None,
            abandoned_at: None,
            owner_session: conversation.map(|c| format!("c{c}")),
            owner_pid: pid,
            resume: super::super::ResumeContext::default(),
        }
    }

    fn caller() -> EngineIdentity {
        EngineIdentity {
            session_id: Some("c0".into()),
            pid: Some(1),
        }
    }

    /// #3858: a failed process check is "cannot tell", not "not yours".
    #[test]
    fn an_unknown_engine_start_marks_a_process_tied_session_as_may_be_yours() {
        let sessions = [session(1, None, Some(1))];
        let text = checkpoint_error(&sessions, &caller(), None);
        assert!(text.contains("may be yours"), "{text}");
        assert!(text.contains("cannot tell"), "{text}");
        assert!(text.contains("Fix: check that /bin/ps works"), "{text}");
        assert!(
            !text.contains("yours: started by this engine process"),
            "{text}"
        );
        assert!(only_own_session(sessions.iter(), &caller(), None).is_none());
    }

    #[test]
    fn a_known_engine_start_still_marks_the_session_as_yours() {
        let sessions = [session(1, None, Some(1))];
        let start = Some(SystemTime::UNIX_EPOCH);
        let text = checkpoint_error(&sessions, &caller(), start);
        assert!(
            text.contains("yours: started by this engine process"),
            "{text}"
        );
        assert!(!text.contains("cannot tell"), "{text}");
        assert!(only_own_session(sessions.iter(), &caller(), start).is_some());
    }

    #[test]
    fn replace_says_cannot_tell_for_a_session_it_cannot_place() {
        let sessions = [session(1, None, Some(1)), session(2, Some(7), None)];
        let err = refuse_unnamed_foreign(&sessions, &caller(), None, &[]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("s1 (cannot tell if it is yours)"), "{text}");
        assert!(text.contains("s2"), "{text}");
        assert!(!text.contains("s2 (cannot tell"), "{text}");
        assert!(text.contains("Fix: check that /bin/ps works"), "{text}");
    }

    #[test]
    fn replace_has_no_cannot_tell_text_when_every_owner_is_known() {
        let sessions = [session(2, Some(7), None)];
        let err = refuse_unnamed_foreign(&sessions, &caller(), None, &[]).unwrap_err();
        assert!(!err.to_string().contains("cannot tell"), "{err}");
    }

    proptest! {
        #[test]
        fn only_one_own_session_is_picked_in_any_order(
            owners in proptest::collection::vec(
                (proptest::option::of(0u8..3), proptest::option::of(1u32..4)),
                0..6,
            ),
            shift in 0usize..6,
        ) {
            let me = EngineIdentity { session_id: Some("c0".into()), pid: Some(1) };
            let sessions: Vec<Session> = owners
                .iter()
                .enumerate()
                .map(|(i, (c, p))| session(i, *c, *p))
                .collect();
            let own: Vec<&str> = owners
                .iter()
                .zip(&sessions)
                .filter(|((c, p), _)| *c == Some(0) || *p == Some(1))
                .map(|(_, s)| s.id.as_str())
                .collect();
            let expected = (own.len() == 1).then(|| own[0]);
            let mut rotated = sessions.clone();
            let len = rotated.len();
            rotated.rotate_left(shift.min(len));
            let since = Some(SystemTime::UNIX_EPOCH);
            for list in [&sessions, &rotated] {
                let picked = only_own_session(list.iter(), &me, since).map(|s| s.id.as_str());
                prop_assert_eq!(picked, expected);
            }
        }
    }
}
