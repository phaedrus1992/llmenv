//! The failure of one tool call attempt, and the status that marks a stale session.

/// Internal result of one [`McpHttpClient::try_call_tool`] attempt: whether a
/// failure is worth clearing the cached session and retrying once (#1094), or
/// is fatal on the first try.
pub(super) enum CallToolError {
    /// The server rejected the cached `Mcp-Session-Id` (HTTP 400/404) —
    /// clearing it and re-initializing may still succeed.
    StaleSession(anyhow::Error),
    /// Any other failure — retrying with a fresh session wouldn't help.
    Fatal(anyhow::Error),
}

impl From<CallToolError> for anyhow::Error {
    fn from(e: CallToolError) -> Self {
        match e {
            CallToolError::StaleSession(e) | CallToolError::Fatal(e) => e,
        }
    }
}

/// Whether `status` is the MCP Streamable HTTP transport's shape for "this
/// session id isn't valid" (mcp-proxy itself uses 400 for "Bad Request:
/// Missing session ID"; a dead/expired session is 404). Over-broad by
/// design: a 400 for an unrelated reason (e.g. malformed tool arguments)
/// also takes the stale-session retry path, wasting one re-initialize
/// before the real error surfaces on the second attempt — but it always
/// does surface, just one round trip later.
pub(super) fn is_stale_session_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::NOT_FOUND
    )
}
