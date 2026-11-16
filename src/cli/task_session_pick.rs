//! Picks the session a `llmenv task session` command acts on when no id is named.

use super::task_guard::guard_session;
use crate::task::ownership::{SessionOverride, ensure_session_is_ours};

/// The session a write command (`session edit`, `note`, `finish`) acts on, and the override note
/// when `--other-session` was needed. An omitted id resolves through
/// [`crate::task::session::pick_open_session`], so with two or more open the caller's own session
/// wins (#2365). One read of the session store serves the pick and the guard (#3743).
pub(super) fn guard_session_for_write(
    state_dir: &std::path::Path,
    project: &str,
    id: Option<String>,
    other_session: Option<&str>,
) -> anyhow::Result<(String, SessionOverride)> {
    match id {
        Some(id) => {
            let audit = guard_session(state_dir, &id, other_session)?;
            Ok((id, audit))
        }
        None => {
            let (picked, caller) = pick_session(state_dir, project)?;
            let audit = ensure_session_is_ours(&picked, &caller, other_session)?;
            Ok((picked, audit))
        }
    }
}

/// Pick the open session of the project, and the caller that the same pick means. One read of
/// the session store serves both (#3743).
fn pick_session(
    state_dir: &std::path::Path,
    project: &str,
) -> anyhow::Result<(String, crate::task::ownership::Caller)> {
    use crate::task::session::{self, EngineIdentity, PickError};
    // Fallible read: an unreadable store must not read as "no open session" (#1112).
    let open = session::open_sessions_strict(state_dir, project)?;
    let owner = EngineIdentity::from_env()?;
    let pick = session::pick_open_session(open, &owner);
    let caller = crate::task::ownership::caller_for_pick(&pick, &owner);
    match pick {
        Ok(session) => Ok((session.id, caller)),
        Err(PickError::NoneOpen) => {
            anyhow::bail!("no open session for this project — pass an id explicitly")
        }
        Err(e) => anyhow::bail!("{}", e.message("pass an id explicitly")),
    }
}

/// The session that a read (`session show`, `session summary`) acts on. An omitted id
/// must not pick another conversation's session, so the pick passes the ownership guard.
/// An explicit id is read as given, because the caller named it (#3588).
pub(super) fn resolve_session_for_read(
    state_dir: &std::path::Path,
    project: &str,
    id: Option<String>,
) -> anyhow::Result<String> {
    if let Some(id) = id {
        return Ok(id);
    }
    let (picked, caller) = pick_session(state_dir, project)?;
    // The read names its own fix. The write guard's text names `--other-session`, which these
    // commands do not take.
    // A read takes no `--other-session`, so the guard makes no note to record.
    let _ = ensure_session_is_ours(&picked, &caller, None).map_err(|_| {
        anyhow::anyhow!(
            "session '{picked}' belongs to another conversation. Fix: pass its id as an argument \
             to read it, for example `llmenv task session show {picked}`"
        )
    })?;
    Ok(picked)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The answer of a pick depends on the engine identity of the host. The test accepts the
    /// answers that the pick documents, and counts the reads.
    fn assert_documented(result: &anyhow::Result<impl Sized>) {
        if let Err(error) = result {
            let text = format!("{error:#}");
            assert!(
                text.contains("belongs to another conversation")
                    || text.contains("no open session")
                    || text.contains("pass an id explicitly")
                    || text.contains("--other-session"),
                "an undocumented failure: {text}"
            );
        }
    }

    fn started_project(dir: &std::path::Path) -> String {
        let project = crate::task::project::current_tag().expect("project tag");
        crate::task::session::start_session(
            dir,
            None,
            None,
            &project,
            crate::task::session::StartDecision::New,
        )
        .expect("start session");
        project
    }

    /// A read of a session without an id picks and checks the caller from one read of the
    /// session store (#3743).
    #[test]
    fn a_read_without_an_id_reads_the_session_store_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = started_project(dir.path());
        let before = crate::task::session::session_reads_on_this_thread();
        let result = resolve_session_for_read(dir.path(), &project, None);
        assert_eq!(
            crate::task::session::session_reads_on_this_thread() - before,
            1
        );
        assert_documented(&result);
    }

    /// A write without an id picks and guards from one read of the session store (#3743).
    #[test]
    fn a_write_without_an_id_reads_the_session_store_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = started_project(dir.path());
        let before = crate::task::session::session_reads_on_this_thread();
        let result = guard_session_for_write(dir.path(), &project, None, None);
        assert_eq!(
            crate::task::session::session_reads_on_this_thread() - before,
            1
        );
        assert_documented(&result);
    }
}
