//! The `session start` checkpoint: which open sessions the caller owns, the resume of one, and the
//! refusal that lists them.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::{EngineIdentity, Session, StartRequest, now_rfc3339, save_session};

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

/// Whether the caller started `session`. The process id alone counts only when the session was
/// active after the caller's engine started, because a process id is reused (#2904).
fn is_callers(session: &Session, owner: &EngineIdentity, engine_start: Option<SystemTime>) -> bool {
    match ownership(session, owner) {
        Ownership::Conversation => true,
        Ownership::Process => engine_start.is_some_and(|start| active_since(session, start)),
        Ownership::None => false,
    }
}

fn active_since(session: &Session, start: SystemTime) -> bool {
    let Some(at) = llmenv_util::time::instant_or_log(
        &session.last_activity,
        "session last_activity",
        &session.id,
    ) else {
        // An unknown value reads as "not active". The refusal that follows is non-destructive, so
        // this default is the safe one. The notice names the cause, so the refusal is not
        // silent about it (#3811).
        llmenv_util::agent_notice::record_logged(unreadable_last_activity(&session.id));
        return false;
    };
    at >= start
}

/// The notice for a `last_activity` that does not parse. The checkpoint and the listing both
/// record it, so they report the same cause (#3859).
fn unreadable_last_activity(session_id: &str) -> String {
    format!(
        "session {session_id} has an unreadable last_activity, so it is not known to be active. \
         Fix: rewrite it as RFC 3339, such as 2026-01-01T00:00:00Z"
    )
}

/// When the engine process `pid` started, from its elapsed time in `ps`. `None` when `ps` cannot
/// tell, and the reason is logged.
pub(super) fn engine_start(pid: u32) -> Option<SystemTime> {
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(["-o", "etime=", "-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = match llmenv_util::process::run_traced("task:ps", cmd, PS_TIMEOUT) {
        Ok(out) if out.status.success() => out,
        // `ps -p` exits 1 with no output for a pid that does not exist (#2923).
        Ok(out)
            if out.status.code() == Some(1) && out.stdout.is_empty() && out.stderr.is_empty() =>
        {
            tracing::error!("ps found no engine process {pid}");
            return None;
        }
        Ok(out) => {
            tracing::error!(
                "/bin/ps failed to read the age of engine process {pid} ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return None;
        }
        Err(e) => {
            tracing::error!("cannot run /bin/ps to read the age of engine process {pid}: {e}");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(age) = parse_etime(text.trim()) else {
        tracing::error!("cannot read the age of engine process {pid} from ps: {text:?}");
        return None;
    };
    // `etime` has a resolution of one second.
    SystemTime::now().checked_sub(age + Duration::from_secs(1))
}

/// `ps` answers in milliseconds. The bound keeps a stuck `ps` from holding the task store lock.
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse the `etime` of `ps`: `[[dd-]hh:]mm:ss`.
fn parse_etime(text: &str) -> Option<Duration> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (number(days, u64::MAX)?, clock),
        None => (0, text),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => (0, number(m, 59)?, number(s, 59)?),
        [h, m, s] => (number(h, 23)?, number(m, 59)?, number(s, 59)?),
        _ => return None,
    };
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hours * 3_600 + minutes * 60 + seconds)?;
    Some(Duration::from_secs(secs))
}

/// An ASCII decimal number of at most 7 digits, no more than `max`.
fn number(text: &str, max: u64) -> Option<u64> {
    let shape_ok = (1..=7).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
    let value: u64 = shape_ok.then(|| text.parse().ok()).flatten()?;
    (value <= max).then_some(value)
}

/// Build the `session start` checkpoint error message: lists every existing
/// open same-project session with enough detail (id, name, description,
/// idle duration, and whether `owner` started it) that the agent or a human
/// can decide `--resume`, `--replace`, or `--new` without needing to inspect
/// anything further.
pub(super) fn checkpoint_error(existing: &[Session], owner: &EngineIdentity) -> String {
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
                ownership_note(s, owner),
            )
        })
        .collect();
    let yours = existing
        .iter()
        .filter(|s| !ownership_note(s, owner).is_empty())
        .count();
    // `--replace` abandons every listed session, so it is only offered as the
    // way to drop "yours" when every listed session is yours.
    let hint = if yours == 0 {
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
    format!(
        "session(s) already open for this project:\n{}\n\
         pass one of --resume <id>, --replace, or --new{hint}",
        lines.join("\n")
    )
}

/// The checkpoint's note on whether `owner` started `session`. The same
/// engine process with a different conversation id is the state after a
/// `/clear` or a compaction (#2365).
/// How the caller owns a listed session.
enum Ownership {
    Conversation,
    Process,
    None,
}

fn ownership(session: &Session, owner: &EngineIdentity) -> Ownership {
    if owner.session_id.is_some() && session.owner_session.as_deref() == owner.session_id.as_deref()
    {
        Ownership::Conversation
    } else if owner.pid.is_some() && session.owner_pid == owner.pid {
        Ownership::Process
    } else {
        Ownership::None
    }
}

fn ownership_note(session: &Session, owner: &EngineIdentity) -> &'static str {
    match ownership(session, owner) {
        Ownership::Conversation => " — yours: started by this conversation",
        Ownership::Process => {
            " — yours: started by this engine process, most likely before a /clear or a compaction"
        }
        Ownership::None => "",
    }
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
pub fn idle_display(last_activity: &str, session_id: &str) -> String {
    let now = std::time::SystemTime::now();
    let Some(at) =
        llmenv_util::time::instant_or_log(last_activity, "session last_activity", session_id)
    else {
        // The listing has no other place for the cause, so the notice carries it (#3859).
        llmenv_util::agent_notice::record_logged(unreadable_last_activity(session_id));
        return "unknown: last_activity is unreadable".to_string();
    };
    match now.duration_since(at) {
        Ok(d) => llmenv_util::time::format_age(std::time::Duration::from_secs(d.as_secs())),
        Err(_) => {
            // Each listing row calls this. A repeat of the notice is not logged again (#3811).
            llmenv_util::agent_notice::record_logged(format!(
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
    let ids: Vec<&str> = unnamed.iter().map(|s| s.id.as_str()).collect();
    let flags: String = ids
        .iter()
        .map(|id| format!(" --other-session {id}"))
        .collect();
    anyhow::bail!(
        "--replace would abandon session(s) of another conversation: {}\n\
         Nothing was changed. To abandon them anyway, run again with{flags}",
        ids.join(", ")
    )
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn etime_reads_every_ps_shape() {
        assert_eq!(parse_etime("00:05"), Some(Duration::from_secs(5)));
        assert_eq!(parse_etime("01:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(
            parse_etime("2-00:00:01"),
            Some(Duration::from_secs(172_801))
        );
    }

    #[test]
    fn etime_refuses_a_malformed_value() {
        for text in [
            "", "5", "1:2:3:4", "00:60", "24:00:00", "a:00", "-1:00", "1-", "00:٣",
        ] {
            assert_eq!(parse_etime(text), None, "{text:?}");
        }
    }

    #[test]
    fn this_process_started_before_now() {
        let start = engine_start(std::process::id()).unwrap();
        assert!(start <= SystemTime::now());
    }

    /// #3811: an unreadable `last_activity` is not active, and the cause is noticed to the user.
    #[test]
    fn an_unreadable_last_activity_is_noticed_and_not_active() {
        let mut s = session(9, None, None);
        s.last_activity = "garbage-3811".into();
        assert!(!active_since(&s, SystemTime::UNIX_EPOCH));
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("unreadable last_activity") && notices.contains("s9"),
            "{notices}"
        );
    }

    #[test]
    fn idle_display_names_an_unreadable_last_activity_in_the_notice() {
        assert_eq!(
            idle_display("yesterday", "s10"),
            "unknown: last_activity is unreadable"
        );
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("unreadable last_activity") && notices.contains("s10"),
            "{notices}"
        );
    }

    #[test]
    fn idle_display_names_a_future_last_activity_in_the_notice() {
        assert_eq!(
            idle_display("2999-01-01T00:00:00Z", "s11"),
            "unknown: last_activity is in the future"
        );
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("last_activity in the future") && notices.contains("s11"),
            "{notices}"
        );
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
                prop_assert!(llmenv_util::time::instant_or_log(&text, "test", "s1").is_some());
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

        #[test]
        fn etime_round_trips(days in 0u64..400, h in 0u64..24, m in 0u64..60, s in 0u64..60) {
            let text = if days > 0 {
                format!("{days}-{h:02}:{m:02}:{s:02}")
            } else {
                format!("{h:02}:{m:02}:{s:02}")
            };
            let expected = Duration::from_secs(days * 86_400 + h * 3_600 + m * 60 + s);
            prop_assert_eq!(parse_etime(&text), Some(expected));
        }
    }
}
