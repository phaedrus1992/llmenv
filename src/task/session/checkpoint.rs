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
    let mut own = existing.by_ref().filter(|s| match ownership(s, owner) {
        Ownership::Conversation => true,
        Ownership::Process => engine_start.is_some_and(|start| active_since(s, start)),
        Ownership::None => false,
    });
    match (own.next(), own.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

fn active_since(session: &Session, start: SystemTime) -> bool {
    humantime::parse_rfc3339(&session.last_activity).is_ok_and(|at| at >= start)
}

/// When the engine process `pid` started, from its elapsed time in `ps`. `None` when `ps` cannot
/// tell, and the reason is logged.
pub(super) fn engine_start(pid: u32) -> Option<SystemTime> {
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(["-o", "etime=", "-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = match crate::process::run_traced("task:ps", cmd, PS_TIMEOUT) {
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
            let idle = idle_display(&s.last_activity);
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
/// `"unknown"` when the timestamp can't be parsed or is in the future.
#[must_use]
pub(crate) fn idle_display(last_activity: &str) -> String {
    let now = std::time::SystemTime::now();
    humantime::parse_rfc3339(last_activity)
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .map(|d| {
            humantime::format_duration(std::time::Duration::from_secs(d.as_secs())).to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
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
        .filter(|s| matches!(ownership(s, owner), Ownership::None))
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
