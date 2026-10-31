//! Redacting `Debug` for the cached OAuth login token.

use super::Credentials;

/// Redacting `Debug` — a derived one would print the access and refresh tokens
/// verbatim into any `{:?}` or `tracing` field. Only the expiry timestamps are
/// safe to show, and they're the only part worth debugging.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("expires_at", &self.expires_at())
            .field("refresh_expires_at", &self.refresh_expires_at())
            .field("token", &"<redacted>")
            .finish()
    }
}
