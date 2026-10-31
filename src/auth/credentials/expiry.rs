//! Expiry checks for the cached OAuth login token.

use super::{Credentials, now_ms};
use crate::materialize::expiry::CLOCK_SKEW_TOLERANCE;

impl Credentials {
    /// True when this blob is worthless: the access token has expired *and* no
    /// live refresh token remains. A stale access token with a live refresh
    /// token is still worth caching — Claude Code renews it on next use.
    ///
    /// A blob with no `expiresAt` is treated as live; an unknown expiry is not
    /// grounds for discarding a credential.
    #[must_use]
    pub(super) fn is_expired(&self, now_ms: i64) -> bool {
        // MCP server tokens are siblings of the login token in the same store and
        // expire independently, so a dead login token must not discard them —
        // that would silently log the user out of every third-party MCP server
        // (Slack, Notion, …). MCP entries carry no expiry of their own; Claude
        // Code refreshes or re-authorizes them per server (#1058).
        if self.mcp_server_count() > 0 {
            return false;
        }
        // Both tokens get the GC skew tolerance (#3098), in one direction only: a token that
        // expired within the tolerance still counts as live. A clock that runs behind still
        // reads a dead token as live. A zero fallback keeps the strict check if the conversion
        // fails.
        let skew_ms = i64::try_from(CLOCK_SKEW_TOLERANCE.as_millis()).unwrap_or(0);
        let access_dead = self
            .expires_at()
            .is_some_and(|t| t.saturating_add(skew_ms) < now_ms);
        let refresh_live = self
            .refresh_expires_at()
            .is_some_and(|t| t.saturating_add(skew_ms) >= now_ms);
        access_dead && !refresh_live
    }

    /// [`Credentials::is_expired`] against the current wall clock.
    #[must_use]
    pub(crate) fn is_expired_now(&self) -> bool {
        self.is_expired(now_ms())
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::super::Credentials;
    use proptest::prelude::*;

    /// Comfortably in the past, so a blob using it is expired under any real clock.
    const PAST_MS: i64 = 1_000_000_000_000;
    /// Comfortably in the future (year ~2096).
    const FUTURE_MS: i64 = 4_000_000_000_000;

    fn blob(expires_at: i64, refresh_expires_at: Option<i64>) -> serde_json::Value {
        let mut oauth = serde_json::json!({
            "accessToken": "sk-ant-oat-TESTONLY",
            "refreshToken": "sk-ant-ort-TESTONLY",
            "expiresAt": expires_at,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
        });
        if let Some(refresh) = refresh_expires_at {
            oauth["refreshTokenExpiresAt"] = refresh.into();
        }
        serde_json::json!({ "claudeAiOauth": oauth })
    }

    /// The skew tolerance in milliseconds, read from the GC constant so the two cannot drift.
    fn skew_ms() -> i64 {
        i64::try_from(crate::materialize::expiry::CLOCK_SKEW_TOLERANCE.as_millis()).expect("test")
    }

    /// One millisecond past the skew tolerance: the first instant a dead token reads as expired.
    fn after_skew_ms() -> i64 {
        PAST_MS + skew_ms() + 1
    }

    /// A blob carrying MCP server tokens is worth keeping even when the login
    /// token is dead — they authenticate different things and expire
    /// independently (#1058). Discarding it would silently log the user out of
    /// every third-party MCP server (Slack, Notion, …).
    #[test]
    fn dead_login_token_with_mcp_tokens_is_not_expired() {
        let mut doc = blob(PAST_MS, Some(PAST_MS));
        doc["mcpOAuth"] = serde_json::json!({
            "notion|0123456789abcdef": { "accessToken": "mcp-TESTONLY" }
        });
        let creds = Credentials::from_json(doc).unwrap();
        assert!(
            !creds.is_expired(PAST_MS + 1),
            "a dead login token must not discard live MCP server tokens"
        );
    }

    /// Without any MCP tokens, a dead login token is still worthless.
    #[test]
    fn dead_login_token_without_mcp_tokens_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, Some(PAST_MS))).unwrap();
        assert!(creds.is_expired(after_skew_ms()));
    }

    /// An empty `mcpOAuth` map is not a reason to keep a dead blob.
    #[test]
    fn empty_mcp_map_does_not_rescue_a_dead_blob() {
        let mut doc = blob(PAST_MS, Some(PAST_MS));
        doc["mcpOAuth"] = serde_json::json!({});
        let creds = Credentials::from_json(doc).unwrap();
        assert!(creds.is_expired(after_skew_ms()));
    }

    #[test]
    fn live_access_token_is_not_expired() {
        let creds = Credentials::from_json(blob(FUTURE_MS, Some(FUTURE_MS))).unwrap();
        assert!(!creds.is_expired(PAST_MS));
    }

    #[test]
    fn stale_access_token_with_live_refresh_is_not_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, Some(FUTURE_MS))).unwrap();
        assert!(!creds.is_expired(PAST_MS + 1));
    }

    #[test]
    fn stale_access_token_without_refresh_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, None)).unwrap();
        assert!(creds.is_expired(after_skew_ms()));
    }

    #[test]
    fn stale_access_token_with_stale_refresh_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, Some(PAST_MS))).unwrap();
        assert!(creds.is_expired(after_skew_ms()));
    }

    /// #3098: exactly at the tolerance edge the token is still live.
    #[test]
    fn access_token_at_skew_edge_is_still_live() {
        let creds = Credentials::from_json(blob(PAST_MS, None)).unwrap();
        assert!(!creds.is_expired(PAST_MS + skew_ms()));
    }

    /// #3098: one millisecond past the edge, the token is dead.
    #[test]
    fn access_token_past_skew_edge_is_expired() {
        let creds = Credentials::from_json(blob(PAST_MS, None)).unwrap();
        assert!(creds.is_expired(after_skew_ms()));
    }

    /// #3098: a refresh token gets the same tolerance, so a dead access token keeps its blob
    /// while the refresh token is within the edge.
    #[test]
    fn refresh_token_at_skew_edge_keeps_the_blob() {
        // The access token expired long before, so only the refresh token can keep the blob.
        let creds = Credentials::from_json(blob(PAST_MS - 100, Some(PAST_MS))).unwrap();
        assert!(!creds.is_expired(PAST_MS + skew_ms()));
    }

    /// #3098: past the edge, a refresh token no longer keeps the blob.
    #[test]
    fn refresh_token_past_skew_edge_drops_the_blob() {
        let creds = Credentials::from_json(blob(PAST_MS - 100, Some(PAST_MS))).unwrap();
        assert!(creds.is_expired(after_skew_ms()));
    }

    proptest! {
        /// Once a blob reads as expired, every later instant reads it as expired too.
        #[test]
        fn expiry_is_monotonic_in_now(
            access in 0i64..4_000_000_000_000,
            refresh in proptest::option::of(0i64..4_000_000_000_000),
            now in 0i64..4_000_000_000_000,
            gap in 0i64..10_000_000,
        ) {
            let creds = Credentials::from_json(blob(access, refresh)).unwrap();
            if creds.is_expired(now) {
                prop_assert!(creds.is_expired(now + gap));
            }
        }

        /// A blob with MCP tokens is never expired, whatever the clock says.
        #[test]
        fn mcp_tokens_keep_any_blob_live(access in any::<i64>(), now in any::<i64>()) {
            let mut doc = blob(access, None);
            doc["mcpOAuth"] = serde_json::json!({
                "notion|0123456789abcdef": { "accessToken": "mcp-TESTONLY" }
            });
            let creds = Credentials::from_json(doc).unwrap();
            prop_assert!(!creds.is_expired(now));
        }
    }

    #[test]
    fn missing_expiry_is_treated_as_live() {
        let creds =
            Credentials::from_json(serde_json::json!({ "claudeAiOauth": { "accessToken": "x" } }))
                .unwrap();
        assert!(!creds.is_expired(FUTURE_MS));
    }
}
