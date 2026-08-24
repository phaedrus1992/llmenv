//! Client side of `launch`'s per-session socket (#1480): checks for a
//! pending mid-session notice (config drift, credential expiry) on every
//! `hook_run` invocation. See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.
//!
//! The wire format (`{"verb": "pending_events"}` request, `{"notice": ...}`
//! response) is duplicated here deliberately rather than shared as a library
//! type with `crate::launch::socket` — the two sides only need to agree on a
//! two-field JSON shape, and sharing a type would pull `launch` into
//! `hook_run`'s dependency graph for no real benefit.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Budget for the whole connect-request-response round trip, matching the
/// v1 `launch` design's connect-then-fall-back guess.
const BUDGET: Duration = Duration::from_millis(50);

/// Longest response this client accepts, matching `launch::socket`'s own
/// `MAX_REQUEST_LEN` — a malformed or hostile endpoint claiming a larger
/// length must not make this client allocate on its say-so.
const MAX_RESPONSE_LEN: u32 = 4096;

/// Check the resident `launch` process (if any) for a pending mid-session
/// notice. Returns `None` for every failure mode — no `LLMENV_LAUNCH_SOCKET`
/// set, no socket file, a connect/IO error, a timeout, or a malformed
/// response — this must never turn into a hook failure.
pub(crate) fn check_pending_notice() -> Option<String> {
    let path = std::env::var_os("LLMENV_LAUNCH_SOCKET")?;
    // #1484: required alongside the socket path — a socket with no matching
    // token means this process didn't inherit it from `launch`, so there is
    // nothing valid to send.
    let token = std::env::var("LLMENV_LAUNCH_TOKEN").ok()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async move {
        tokio::time::timeout(BUDGET, fetch(path, token))
            .await
            .ok()
            .flatten()
    })
}

async fn fetch(path: std::ffi::OsString, token: String) -> Option<String> {
    let mut stream = UnixStream::connect(path).await.ok()?;

    // Symmetric to launch::socket's own peer check: refuse to trust a
    // responder running as a different uid, in case whatever bound this
    // path isn't the real `launch` process. Doesn't distinguish a
    // different process at the same uid — the token below is what covers
    // that case (#1484).
    let peer_uid = stream.peer_cred().ok()?.uid();
    let my_uid = rustix::process::geteuid().as_raw();
    if !crate::launch::socket::is_authorized_peer(peer_uid, my_uid) {
        return None;
    }

    let request = serde_json::json!({ "verb": "pending_events", "token": token });
    let bytes = serde_json::to_vec(&request).ok()?;
    let len: u32 = bytes.len().try_into().ok()?;
    stream.write_all(&len.to_be_bytes()).await.ok()?;
    stream.write_all(&bytes).await.ok()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_RESPONSE_LEN {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await.ok()?;
    let response: serde_json::Value = serde_json::from_slice(&buf).ok()?;
    response.get("notice")?.as_str().map(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_when_env_var_is_unset() {
        // Not asserting via a real env mutation (unsafe, forbidden
        // workspace-wide, and races other tests) — a missing
        // `LLMENV_LAUNCH_SOCKET` is already the default state for a test
        // process, since nothing else in this suite sets it.
        assert_eq!(std::env::var_os("LLMENV_LAUNCH_SOCKET"), None);
        assert_eq!(check_pending_notice(), None);
    }

    #[tokio::test]
    async fn delivers_a_queued_notice_from_a_real_socket() {
        let (listener, notices, path, token) =
            crate::launch::socket::bind(std::process::id() + 1).unwrap();
        *notices.lock().await = Some("credentials expire soon".to_string());
        let server = tokio::spawn(crate::launch::socket::serve(
            listener,
            notices,
            token.clone(),
        ));

        let notice_token = token.as_str().to_string();
        let notice =
            tokio::task::spawn_blocking(move || fetch_with_env_override(&path, notice_token))
                .await
                .unwrap();

        assert_eq!(notice, Some("credentials expire soon".to_string()));
        server.abort();
    }

    /// #1484: a request carrying the wrong token must not receive the
    /// notice, even though the peer uid check passes (same test process).
    #[tokio::test]
    async fn rejects_a_notice_fetch_with_the_wrong_token() {
        let (listener, notices, path, token) =
            crate::launch::socket::bind(std::process::id() + 5).unwrap();
        *notices.lock().await = Some("credentials expire soon".to_string());
        let server = tokio::spawn(crate::launch::socket::serve(listener, notices, token));

        let notice = fetch(path.into_os_string(), "wrong-token".to_string()).await;
        assert_eq!(notice, None);
        server.abort();
    }

    /// [`check_pending_notice`] reads `LLMENV_LAUNCH_SOCKET` and
    /// `LLMENV_LAUNCH_TOKEN` from the real process env, which this test
    /// can't safely mutate (see the module's other test). Exercise the same
    /// connect-request-response path directly against `path`/`token`
    /// instead, via a fresh runtime on a blocking thread — mirroring exactly
    /// what `check_pending_notice` itself does, minus the env lookup.
    fn fetch_with_env_override(path: &std::path::Path, token: String) -> Option<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(async {
            tokio::time::timeout(BUDGET, fetch(path.as_os_str().to_owned(), token))
                .await
                .ok()
                .flatten()
        })
    }

    /// A valid, complete JSON response `{"notice":"<padding>"}` whose total
    /// serialized length is exactly `len` bytes — built by construction
    /// (fixed prefix/suffix, padding fills the rest) rather than trial and
    /// error, so tests can hit an exact byte boundary. `len` must be at
    /// least the 13-byte fixed overhead (`{"notice":"` + `"}`).
    fn response_payload_of_exact_len(len: u32) -> Vec<u8> {
        const PREFIX: &[u8] = b"{\"notice\":\"";
        const SUFFIX: &[u8] = b"\"}";
        let overhead = u32::try_from(PREFIX.len() + SUFFIX.len()).unwrap();
        let padding_len = usize::try_from(len - overhead).unwrap();
        let mut payload = Vec::with_capacity(len as usize);
        payload.extend_from_slice(PREFIX);
        payload.extend(std::iter::repeat_n(b'x', padding_len));
        payload.extend_from_slice(SUFFIX);
        assert_eq!(payload.len(), len as usize);
        payload
    }

    /// Serve exactly one request, then send `payload` prefixed by its own
    /// (real, matching) length — i.e. an honest response of that size, not
    /// a length header lying about a payload that never arrives. Needed to
    /// tell "rejected because it's oversized" apart from "failed because
    /// the connection closed before the promised bytes showed up", which a
    /// truncated fake response can't distinguish.
    fn serve_one_response(
        listener: tokio::net::UnixListener,
        payload: Vec<u8>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let request_len = u32::from_be_bytes(len_buf) as usize;
            let mut discard = vec![0u8; request_len];
            stream.read_exact(&mut discard).await.unwrap();

            let payload_len = u32::try_from(payload.len()).unwrap();
            stream.write_all(&payload_len.to_be_bytes()).await.unwrap();
            stream.write_all(&payload).await.unwrap();
        })
    }

    /// A complete, honestly-sized response of exactly [`MAX_RESPONSE_LEN`]
    /// bytes must be accepted — the boundary itself is not oversized.
    #[tokio::test]
    async fn accepts_a_response_of_exactly_the_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let payload = response_payload_of_exact_len(MAX_RESPONSE_LEN);
        let server = serve_one_response(listener, payload);

        let notice = fetch(path.into_os_string(), "any-token".to_string()).await;
        assert!(
            notice.is_some(),
            "an exactly-max-length response must not be rejected"
        );
        server.abort();
    }

    /// A complete, honestly-sized response one byte past
    /// [`MAX_RESPONSE_LEN`] must be rejected before this client reads (or
    /// returns) any of it — proven with a real, fully-sent oversized
    /// payload rather than a truncated one, so a mutant that weakens the
    /// length check can't hide behind a coincidental connection-closed
    /// `None`.
    #[tokio::test]
    async fn rejects_a_response_one_byte_past_the_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let payload = response_payload_of_exact_len(MAX_RESPONSE_LEN + 1);
        let server = serve_one_response(listener, payload);

        let notice = fetch(path.into_os_string(), "any-token".to_string()).await;
        assert_eq!(notice, None);
        server.abort();
    }

    proptest::proptest! {
        /// The wire protocol (length-prefixed JSON) must round-trip an
        /// arbitrary notice byte-for-byte through the real server
        /// (`launch::socket::bind`/`serve`) and client (`fetch`) together —
        /// not just prove `serde_json` itself round-trips, which was never
        /// in question. Bounded to comfortably under both sides' 4096-byte
        /// length cap once JSON-encoded.
        #[test]
        fn wire_protocol_roundtrips_arbitrary_notices(notice in "\\PC{0,200}") {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (listener, notices, path, token) =
                    crate::launch::socket::bind(std::process::id() + 1000).unwrap();
                *notices.lock().await = Some(notice.clone());
                let server = tokio::spawn(crate::launch::socket::serve(
                    listener,
                    notices,
                    token.clone(),
                ));

                let received =
                    fetch(path.clone().into_os_string(), token.as_str().to_string()).await;

                server.abort();
                let _ = std::fs::remove_file(&path);
                assert_eq!(received, Some(notice));
            });
        }
    }
}
