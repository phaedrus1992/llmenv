//! Who owns a listed session: the caller's conversation, the caller's engine process, or neither.
//! The checkpoint and the `--replace` guard both read it.

use std::time::SystemTime;

use crate::task::session::{EngineIdentity, Session};

/// Whether the caller started `session`. The process id alone counts only when the session was
/// active after the caller's engine started, because a process id is reused (#2904).
pub(super) fn is_callers(
    session: &Session,
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
) -> bool {
    match ownership(session, owner, engine_start) {
        Ownership::Conversation => true,
        Ownership::Process(start) => active_since(session, start),
        Ownership::Unknown | Ownership::None => false,
    }
}

fn active_since(session: &Session, start: SystemTime) -> bool {
    let Some(at) = crate::util::time::instant_or_log(
        &session.last_activity,
        "session last_activity",
        crate::util::time::RecordId(&session.id),
    ) else {
        // An unknown value reads as "not active". The refusal that follows is non-destructive, so
        // this default is the safe one. The notice names the cause, so the refusal is not
        // silent about it (#3811).
        crate::agent_notice::record_logged(unreadable_last_activity(&session.id));
        return false;
    };
    at >= start
}

/// The notice for a `last_activity` that does not parse. The checkpoint and the listing both
/// record it, so they report the same cause (#3859).
pub(super) fn unreadable_last_activity(session_id: &str) -> String {
    format!(
        "session {session_id} has an unreadable last_activity, so it is not known to be active. \
         Fix: rewrite it as RFC 3339, such as 2026-01-01T00:00:00Z"
    )
}

/// How the caller owns a listed session.
pub(super) enum Ownership {
    Conversation,
    /// The process id ties the session to the caller. Carries the start time of the engine
    /// process, because a process id is reused (#2904).
    Process(SystemTime),
    /// The process id ties the session to the caller, but the start time of the engine process
    /// could not be read. The session is not known to be the caller's, and not known to be
    /// another's (#3858).
    Unknown,
    None,
}

pub(super) fn ownership(
    session: &Session,
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
) -> Ownership {
    if owner.session_id.is_some() && session.owner_session.as_deref() == owner.session_id.as_deref()
    {
        Ownership::Conversation
    } else if owner.pid.is_some() && session.owner_pid == owner.pid {
        engine_start.map_or(Ownership::Unknown, Ownership::Process)
    } else {
        Ownership::None
    }
}

const UNKNOWN_OWNER_NOTE: &str = " — may be yours: llmenv could not read when this engine process \
     started, so it cannot tell";

/// The checkpoint's note on whether `owner` started `session`. The same engine process with a
/// different conversation id is the state after a `/clear` or a compaction (#2365).
pub(super) fn ownership_note(
    session: &Session,
    owner: &EngineIdentity,
    engine_start: Option<SystemTime>,
) -> &'static str {
    match ownership(session, owner, engine_start) {
        Ownership::Conversation => " — yours: started by this conversation",
        Ownership::Process(_) => {
            " — yours: started by this engine process, most likely before a /clear or a compaction"
        }
        Ownership::Unknown => UNKNOWN_OWNER_NOTE,
        Ownership::None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_activity(last_activity: &str) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": "s9",
            "name": null,
            "project": "p",
            "started_at": "2026-01-01T00:00:00Z",
            "last_activity": last_activity,
        }))
        .expect("a minimal session parses")
    }

    /// #3811: an unreadable `last_activity` is not active, and the cause is noticed to the user.
    #[test]
    fn an_unreadable_last_activity_is_noticed_and_not_active() {
        let s = session_with_activity("garbage-3811");
        assert!(!active_since(&s, SystemTime::UNIX_EPOCH));
        crate::agent_notice::assert_noticed(&["unreadable last_activity", "s9"]);
    }
}
