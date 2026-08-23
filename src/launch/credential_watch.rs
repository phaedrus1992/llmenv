//! Credential-expiry detection for `launch` (#1285, narrowed scope): notices
//! when the cached Claude Code OAuth credential is close to expiry.
//! Detection and notice only — llmenv has no OAuth refresh call of its own
//! today (Claude Code performs its own refresh; llmenv only caches the
//! result). See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::PathBuf;
use std::time::Duration;

use crate::auth::credentials::Credentials;
use crate::launch::socket::NoticeSlot;

pub(crate) const EXPIRY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const EXPIRY_WARNING_THRESHOLD: Duration = Duration::from_secs(300);

const EXPIRY_NOTICE: &str =
    "credentials expire soon; run `llmenv login` if the engine reports an auth failure.";

/// Whether `creds` expires within `threshold` of `now_unix_ms`, or has
/// already expired. `now_unix_ms` is a parameter (not read internally) so
/// this stays a pure function the unit tests can drive directly.
pub(crate) fn is_near_expiry(creds: &Credentials, threshold: Duration, now_unix_ms: i64) -> bool {
    let Some(expires_at) = creds.expires_at() else {
        return false;
    };
    let threshold_ms: i64 = threshold.as_millis().try_into().unwrap_or(i64::MAX);
    expires_at.saturating_sub(now_unix_ms) <= threshold_ms
}

/// Poll the cached credential every `interval` and queue a notice the first
/// time it's inside the warning threshold. Runs until the caller
/// drops/aborts this task. Fail-soft: a read error or an absent cache is
/// treated as "nothing to warn about," not an error — most sessions have no
/// cached credential at all (e.g. an API-key-only setup), and that is not
/// itself a problem.
pub(crate) async fn watch(adapter_root: PathBuf, notices: NoticeSlot, interval: Duration) {
    let mut interval = tokio::time::interval(interval);
    let mut already_notified = false;
    loop {
        interval.tick().await;
        if already_notified {
            continue;
        }
        let root = adapter_root.clone();
        let creds =
            match tokio::task::spawn_blocking(move || crate::auth::credentials::load_cached(&root))
                .await
            {
                Ok(Ok(Some(creds))) => creds,
                Ok(Ok(None)) => continue,
                Ok(Err(e)) => {
                    tracing::debug!("launch: credential expiry check failed: {e:#}");
                    continue;
                }
                Err(e) => {
                    tracing::debug!("launch: credential expiry check task panicked: {e:#}");
                    continue;
                }
            };
        let now_unix_ms = now_unix_millis();
        if is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now_unix_ms) {
            *notices.lock().await = Some(EXPIRY_NOTICE.to_string());
            already_notified = true;
        }
    }
}

/// Current wall-clock time in Unix milliseconds. Saturates to `0` on a
/// clock before the epoch rather than panicking — this is a warning-timing
/// input, not a security boundary, so a wrong-but-safe value beats a crash.
fn now_unix_millis() -> i64 {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn creds_expiring_at(expires_at_ms: i64) -> Credentials {
        Credentials::from_json(serde_json::json!({
            "claudeAiOauth": { "accessToken": "x", "expiresAt": expires_at_ms }
        }))
        .unwrap()
    }

    #[test]
    fn is_near_expiry_true_when_inside_the_threshold_window() {
        let now = 1_000_000_000_000_i64;
        let creds = creds_expiring_at(now + 60_000); // 60s from now
        assert!(is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now));
    }

    #[test]
    fn is_near_expiry_false_when_outside_the_threshold_window() {
        let now = 1_000_000_000_000_i64;
        let creds = creds_expiring_at(now + 3_600_000); // 1h from now
        assert!(!is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now));
    }

    #[test]
    fn is_near_expiry_true_when_already_expired() {
        let now = 1_000_000_000_000_i64;
        let creds = creds_expiring_at(now - 1_000);
        assert!(is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now));
    }

    #[tokio::test]
    async fn watch_queues_a_notice_for_a_soon_to_expire_credential() {
        let dir = tempfile::tempdir().unwrap();
        let adapter_root = dir.path().join("claude-code");
        let creds = creds_expiring_at(now_unix_millis() + 10_000);
        crate::auth::credentials::save_cached(&adapter_root, &creds).unwrap();

        let notices: NoticeSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let handle = tokio::spawn(watch(
            adapter_root,
            std::sync::Arc::clone(&notices),
            Duration::from_millis(20),
        ));

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            notices.lock().await.as_deref(),
            Some(EXPIRY_NOTICE),
            "expected an expiry notice to be queued"
        );

        handle.abort();
    }
}
