//! The link from a llmenv session to its ICM transcript (#2667).
//!
//! The session map records the link. A transcript starts only when the map can record it, so
//! no transcript is left without a link.

use tracing::error;

use crate::config::SessionLog;
use crate::hook_run::notice;
use crate::session_log::dispatch as transcript_dispatch;
use crate::session_log::{ScopeContext, scope_metadata_json, state};
use llmenv_config::LogLevel;
use llmenv_mcp::mcp_client::McpHttpClient;

/// Reuse a previously-recorded transcript session for `csid`, or — when
/// `cfg.transcript` and a client is available — start a new one and persist
/// the correlation. Returns `None` when transcript logging is unavailable and
/// nothing was recorded before.
///
/// `verify`: whether to revalidate a cached id against ICM before trusting it
/// (#1090) rather than treating its presence alone as proof of liveness — the
/// #1085 failure mode (a wrong cached value trusted forever because the only
/// check that would notice consulted the record itself). Revalidation costs a
/// full-transcript fetch (`icm_transcript_show` has no cheap existence-only
/// form), so only the `SessionStart` caller — once per launch — passes
/// `true`; the per-event caller in `run_session_log` passes `false` to avoid
/// turning every logged tool call into an ICM round trip that grows with the
/// transcript itself.
pub(super) async fn ensure_transcript_session(
    cfg: &SessionLog,
    client: Option<&McpHttpClient>,
    csid: &str,
    ctx: &ScopeContext,
    state_path: Option<&std::path::Path>,
    verify: bool,
) -> Option<String> {
    let path = state_path?;
    let now = super::session_state::unix_now();
    let seen = match state::link_at(path, csid, now) {
        Ok(link) => link,
        Err(e) => {
            // Starting a new transcript here would orphan it: the map cannot record the link (#2658).
            crate::agent_notice::record(notice::transcript_unlinked_notice(&e));
            return None;
        }
    };
    match &seen {
        state::Link::Linked(existing) => {
            if let Some(id) = cached_transcript_id(client, existing, verify).await {
                return Some(id);
            }
        }
        // Another hook is starting the transcript, or its link was lost. A second start would
        // leave a second orphan, so this event waits for the marker to expire (#2834).
        state::Link::Pending { since } => {
            note_pending(*since, now);
            return None;
        }
        state::Link::Missing => {}
    }
    if !cfg.transcript_wants(LogLevel::Info) {
        return None;
    }
    let Some(client) = client else {
        error!("no ICM transcript started: transcripts are on, but no MCP client is configured");
        return None;
    };
    start_linked(client, ctx, path, csid, &seen, now).await
}

/// Claim the session with a pending marker, start the transcript, and record its link. `None`
/// after each failure, which is logged and noticed.
async fn start_linked(
    client: &McpHttpClient,
    ctx: &ScopeContext,
    path: &std::path::Path,
    csid: &str,
    seen: &state::Link,
    now: i64,
) -> Option<String> {
    // The claim also proves the map can be written before ICM starts a transcript. A transcript
    // that cannot be linked is an orphan, and each later event would add another one (#2667).
    match state::claim_pending_at(path, csid, seen, now) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!("another hook claimed the ICM transcript start for this session");
            return None;
        }
        Err(e) => {
            error!(error = %e, "no ICM transcript started: the session map cannot be written");
            crate::agent_notice::record(notice::problem_notice(
                &format!(
                    "no ICM transcript started for this session: the session map cannot be written: {e:#}"
                ),
                "run `llmenv doctor` to check the state directory.",
            ));
            return None;
        }
    }
    let metadata = scope_metadata_json(ctx);
    let id = match transcript_dispatch::start_session(
        client,
        &ctx.adapter,
        ctx.project.as_deref(),
        &metadata,
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "failed to start ICM transcript session");
            let blocked = match state::clear_pending_at(path, csid) {
                Ok(()) => "",
                Err(clear) => {
                    error!(error = %clear, "cannot clear the pending transcript marker; it expires");
                    ". A new start is blocked for up to 10 minutes, because its marker could not be \
                     cleared"
                }
            };
            crate::agent_notice::record(notice::problem_notice(
                &format!("the ICM transcript for this session could not start: {e:#}{blocked}"),
                "run `llmenv doctor` to check the memory server.",
            ));
            return None;
        }
    };
    if let Err(e) = state::record_session_at(path, csid, &id) {
        error!(
            error = %e,
            session_id = %id,
            "ICM transcript started but its link was not recorded, so this event is not logged"
        );
        crate::agent_notice::record(notice::problem_notice(
            &format!(
                "ICM transcript {id} started, but its link to this session was not recorded, so \
                 this event is not logged and transcript {id} has no link. No other transcript \
                 starts for this session for 10 minutes: {e:#}"
            ),
            &format!(
                "run `llmenv doctor` to check the state directory, then remove transcript {id} in ICM."
            ),
        ));
        return None;
    }
    Some(id)
}

/// A start takes seconds. A marker older than this was most likely left by a hook that was
/// stopped mid-start, so the agent is told.
const STALE_PENDING_SECS: i64 = 60;

/// Log and notice a pending transcript link that is older than a start takes. Events are not
/// sent to the ICM transcript while it stands (#2834).
pub(super) fn note_pending(since: i64, now: i64) {
    let age = now.saturating_sub(since);
    if age < STALE_PENDING_SECS {
        tracing::debug!("ICM transcript start pending for {age} s; event not logged");
        return;
    }
    error!("ICM transcript link pending for {age} s; events are not logged to the transcript");
    crate::agent_notice::record(notice::problem_notice(
        &format!(
            "the ICM transcript link for this session has been pending for {age} s, so events are \
             not sent to the ICM transcript. A hook that stopped during the start leaves this \
             marker. It expires 10 minutes after it was written, and the next session start tries \
             again"
        ),
        "run `llmenv doctor` to check the memory server.",
    ));
}

/// Decide whether a recorded transcript id still serves this session. `Some` keeps the link.
/// `None` means ICM confirmed the session is gone, so the caller starts a new one.
///
/// A transport failure keeps the link. ICM being unreachable says nothing about the session,
/// and a new transcript would orphan the old one (#2667).
async fn cached_transcript_id(
    client: Option<&McpHttpClient>,
    existing: &str,
    verify: bool,
) -> Option<String> {
    let Some(client) = client.filter(|_| verify) else {
        return Some(existing.to_owned());
    };
    match transcript_dispatch::verify_session(client, existing).await {
        Ok(transcript_dispatch::Liveness::Live) => Some(existing.to_owned()),
        Ok(transcript_dispatch::Liveness::Gone) => {
            error!(
                session_id = %existing,
                "cached ICM transcript session no longer exists, re-establishing"
            );
            None
        }
        Err(e) => {
            error!(
                error = %e,
                session_id = %existing,
                "cannot verify the cached ICM transcript session, keeping its link"
            );
            Some(existing.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{STALE_PENDING_SECS, note_pending};

    #[test]
    fn only_a_stale_pending_link_is_noticed() {
        note_pending(5_000, 5_000 + STALE_PENDING_SECS - 1);
        let quiet = crate::agent_notice::current().unwrap_or_default();
        assert!(!quiet.contains("pending for 59 s"), "{quiet}");
        note_pending(5_000, 5_000 + STALE_PENDING_SECS);
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("pending for 60 s"), "{notices}");
    }
}
