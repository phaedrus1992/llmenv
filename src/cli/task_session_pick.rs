//! Picks the session a `llmenv task session` command acts on when no id is named.

use crate::task::ownership::ensure_session_is_ours;

/// Resolve an explicit-or-omitted session id the same way `add_task` does:
/// omitted resolves through [`crate::task::session::pick_open_session`], so
/// with two or more open the caller's own session wins (#2365).
pub(super) fn resolve_session_id(
    state_dir: &std::path::Path,
    project: &str,
    id: Option<String>,
) -> anyhow::Result<String> {
    if let Some(id) = id {
        return Ok(id);
    }
    let (picked, _caller) = pick_session(state_dir, project)?;
    Ok(picked)
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
        Err(e) => anyhow::bail!(
            "{}",
            e.ambiguity_message("pass an id explicitly")
                .unwrap_or_else(|| "more than one session is open".to_string())
        ),
    }
}

/// [`resolve_session_id`] for a read (`session show`, `session summary`). An omitted id
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
    ensure_session_is_ours(&picked, &caller, None).map_err(|_| {
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

    /// A read of a session without an id picks and checks the caller from one read of the
    /// session store (#3743). The answer depends on the engine identity of the host, so the test
    /// counts the reads and not the answer.
    #[test]
    fn a_read_without_an_id_reads_the_session_store_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = crate::task::project::current_tag().expect("project tag");
        crate::task::session::start_session(
            dir.path(),
            None,
            None,
            &project,
            crate::task::session::StartDecision::New,
        )
        .expect("start session");
        let before = crate::task::session::session_reads_on_this_thread();
        let _ = resolve_session_for_read(dir.path(), &project, None);
        assert_eq!(
            crate::task::session::session_reads_on_this_thread() - before,
            1
        );
    }
}
