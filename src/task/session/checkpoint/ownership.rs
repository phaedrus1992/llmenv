//! Who owns a listed session: the caller's conversation, the caller's engine process, or neither.
//! The checkpoint and the `--replace` guard both read it, so the note, the count, and the decision
//! cannot disagree.

use std::time::SystemTime;

use crate::task::session::{EngineIdentity, Session};
use crate::util::proc_start::process_start;
use crate::util::time::RecordId;

/// How the caller owns a listed session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ownership {
    /// The conversation of the caller started it.
    Conversation,
    /// The engine process of the caller started it, and it was active after that process started.
    /// A process id is reused, so a session from before that time is not the caller's (#2904).
    Process,
    /// The process id ties it to the caller, but the start time of the engine process could not be
    /// read, so llmenv cannot tell if it is the caller's (#3858).
    UnknownEngineStart,
    /// The process id ties it to the caller, but the `last_activity` of the session is not RFC
    /// 3339, so llmenv cannot tell if it is from before the engine process started (#3858).
    UnknownLastActivity,
    None,
}

impl Ownership {
    /// The cause that llmenv cannot tell, for the line of a session in a refusal. `None` when the
    /// owner is known.
    pub(super) fn cannot_tell_reason(self) -> Option<&'static str> {
        match self {
            Self::UnknownEngineStart => Some("the start time of this engine process is unknown"),
            Self::UnknownLastActivity => Some("its last_activity is unreadable or in the future"),
            _ => None,
        }
    }

    /// Whether the caller started the session.
    pub(super) fn is_callers(self) -> bool {
        matches!(self, Self::Conversation | Self::Process)
    }
}

/// Whether `session` was active at or after `start`. `None` when its `last_activity` is not
/// readable or is in the future, and the cause is noticed.
fn active_since(session: &Session, start: SystemTime) -> Option<bool> {
    last_activity_at(&session.last_activity, &session.id, SystemTime::now())
        .ok()
        .map(|at| at >= start)
}

/// The instant of a `last_activity`. `Err` holds the reason it has no usable instant, in words
/// that finish "last_activity is ...", and the cause is noticed. A value in the future is not
/// usable: it would count as active after any engine start.
pub(super) fn last_activity_at(
    text: &str,
    session_id: &str,
    now: SystemTime,
) -> Result<SystemTime, &'static str> {
    let Some(at) =
        crate::util::time::instant_or_log(text, "session last_activity", RecordId(session_id))
    else {
        crate::agent_notice::record_logged(unreadable_last_activity(session_id));
        return Err("unreadable");
    };
    if at > now {
        // Each listing row and each ownership check calls this. A repeat is not logged again.
        crate::agent_notice::record_logged(format!(
            "session {session_id} has a last_activity in the future, so its idle time is \
             unknown. Fix: check the clock of the machine that wrote it"
        ));
        return Err("in the future");
    }
    Ok(at)
}

/// The notice for a `last_activity` that does not parse. The checkpoint and the listing both
/// record it, so they report the same cause (#3859).
fn unreadable_last_activity(session_id: &str) -> String {
    format!(
        "session {session_id} has an unreadable last_activity, so it is not known to be active. \
         Fix: rewrite it as RFC 3339, such as 2026-01-01T00:00:00Z"
    )
}

/// Whether the caller started or resumed `session`, for the pick and the write guards (#3905). It
/// is the rule of the checkpoint: the conversation id matches, or the engine process id matches and
/// the session was active after that process started, so a session that the caller started before a
/// `/clear` or a compaction still counts. A result of "cannot tell" is not ownership, because a
/// guard must not widen on a guess. `engine_start` is read once, and only when it decides.
pub(crate) fn is_callers_session(
    session: &Session,
    owner: &EngineIdentity,
    engine_start: &std::cell::OnceCell<Option<SystemTime>>,
) -> bool {
    match ownership(session, owner, None) {
        Ownership::Conversation => true,
        Ownership::UnknownEngineStart => {
            let start = *engine_start.get_or_init(|| owner.pid.and_then(process_start));
            ownership(session, owner, start).is_callers()
        }
        _ => false,
    }
}

pub(super) fn ownership(
    session: &Session,
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
) -> Ownership {
    if owner.session_id.is_some() && session.owner_session.as_deref() == owner.session_id.as_deref()
    {
        return Ownership::Conversation;
    }
    if owner.pid.is_none() || session.owner_pid != owner.pid {
        return Ownership::None;
    }
    let Some(start) = engine_start else {
        return Ownership::UnknownEngineStart;
    };
    match active_since(session, start) {
        Some(true) => Ownership::Process,
        Some(false) => Ownership::None,
        None => Ownership::UnknownLastActivity,
    }
}

/// The checkpoint's note on whether the caller started a session. The same engine process with a
/// different conversation id is the state after a `/clear` or a compaction (#2365).
pub(super) fn ownership_note(owned: Ownership) -> &'static str {
    match owned {
        Ownership::Conversation => " — yours: started by this conversation",
        Ownership::Process => {
            " — yours: started by this engine process, most likely before a /clear or a compaction"
        }
        Ownership::UnknownEngineStart => {
            " — may be yours: llmenv could not read when this engine process started, so it \
             cannot tell. The process may be gone, or /bin/ps may not list it"
        }
        Ownership::UnknownLastActivity => {
            " — may be yours: its last_activity is unreadable or in the future, so llmenv cannot \
             tell if it is from before this engine process started"
        }
        Ownership::None => "",
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn session_with(
        last_activity: &str,
        owner_session: Option<&str>,
        owner_pid: Option<u32>,
    ) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": "s9",
            "name": null,
            "project": "p",
            "started_at": "2026-01-01T00:00:00Z",
            "last_activity": last_activity,
            "owner_session": owner_session,
            "owner_pid": owner_pid,
        }))
        .expect("a minimal session parses")
    }

    proptest! {
        /// A `last_activity` is usable exactly when it parses and is not after `now`.
        #[test]
        fn last_activity_is_usable_exactly_when_it_parses_and_is_not_in_the_future(
            at in 0i64..4_000_000_000,
            now in 0i64..4_000_000_000,
        ) {
            let text = jiff::Timestamp::from_second(at).expect("in range").to_string();
            let now_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(now.cast_unsigned());
            let read = last_activity_at(&text, "s9", now_at);
            if at <= now {
                let want = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at.cast_unsigned());
                prop_assert_eq!(read, Ok(want));
            } else {
                prop_assert_eq!(read, Err("in the future"));
            }
        }
    }

    /// #3811: an unreadable `last_activity` is noticed, and the session is not known to be active.
    #[test]
    fn an_unreadable_last_activity_is_noticed_and_cannot_be_told() {
        let s = session_with("garbage-3811", None, None);
        assert_eq!(active_since(&s, SystemTime::UNIX_EPOCH), None);
        crate::agent_notice::assert_noticed(&["unreadable last_activity", "s9"]);
    }

    /// #3858: a session from before the engine started is not the caller's, and the note does not
    /// call it yours.
    #[test]
    fn a_session_older_than_the_engine_is_not_the_callers() {
        let s = session_with("2026-01-01T00:00:00Z", None, Some(1));
        let owner = EngineIdentity {
            session_id: None,
            pid: Some(1),
        };
        let later = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000);
        assert_eq!(ownership(&s, &owner, Some(later)), Ownership::None);
        assert_eq!(ownership_note(Ownership::None), "");
    }

    #[test]
    fn an_unreadable_last_activity_on_a_pid_match_reads_as_cannot_tell() {
        let s = session_with("garbage-3858", None, Some(1));
        let owner = EngineIdentity {
            session_id: None,
            pid: Some(1),
        };
        assert_eq!(
            ownership(&s, &owner, Some(SystemTime::UNIX_EPOCH)),
            Ownership::UnknownLastActivity
        );
    }

    proptest! {
        // The four precedence rules, over every combination of identity, owner fields, and engine
        // start: a conversation match wins, a pid mismatch is never the caller's, an unknown
        // engine start is "cannot tell", and a decision and its note agree.
        #[test]
        fn ownership_follows_its_precedence_rules(
            owner_conv in proptest::option::of(0u8..3),
            owner_pid in proptest::option::of(1u32..4),
            conv in proptest::option::of(0u8..3),
            pid in proptest::option::of(1u32..4),
            start_known in any::<bool>(),
            activity_valid in any::<bool>(),
        ) {
            let activity = if activity_valid { "2026-01-01T00:00:00Z" } else { "bad-3858" };
            let conv_text = conv.map(|c| format!("c{c}"));
            let s = session_with(activity, conv_text.as_deref(), pid);
            let owner = EngineIdentity {
                session_id: owner_conv.map(|c| format!("c{c}")),
                pid: owner_pid,
            };
            let start = start_known.then_some(SystemTime::UNIX_EPOCH);
            let got = ownership(&s, &owner, start);
            let conv_match = owner_conv.is_some() && owner_conv == conv;
            let pid_match = owner_pid.is_some() && owner_pid == pid;
            if conv_match {
                prop_assert_eq!(got, Ownership::Conversation);
            } else if !pid_match {
                prop_assert_eq!(got, Ownership::None);
            } else if !start_known {
                prop_assert_eq!(got, Ownership::UnknownEngineStart);
            } else if activity_valid {
                prop_assert_eq!(got, Ownership::Process);
            } else {
                prop_assert_eq!(got, Ownership::UnknownLastActivity);
            }
            prop_assert_eq!(ownership_note(got).contains("yours: started"), got.is_callers());
        }
    }
}
