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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async move {
        tokio::time::timeout(BUDGET, fetch(path))
            .await
            .ok()
            .flatten()
    })
}

async fn fetch(path: std::ffi::OsString) -> Option<String> {
    let mut stream = UnixStream::connect(path).await.ok()?;
    let request = serde_json::json!({ "verb": "pending_events" });
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
        let (listener, notices, path) =
            crate::launch::socket::bind(std::process::id() + 1).unwrap();
        *notices.lock().await = Some("credentials expire soon".to_string());
        let server = tokio::spawn(crate::launch::socket::serve(listener, notices));

        let notice = tokio::task::spawn_blocking(move || fetch_with_env_override(&path))
            .await
            .unwrap();

        assert_eq!(notice, Some("credentials expire soon".to_string()));
        server.abort();
    }

    /// [`check_pending_notice`] reads `LLMENV_LAUNCH_SOCKET` from the real
    /// process env, which this test can't safely mutate (see the module's
    /// other test). Exercise the same connect-request-response path
    /// directly against `path` instead, via a fresh runtime on a blocking
    /// thread — mirroring exactly what `check_pending_notice` itself does,
    /// minus the env lookup.
    fn fetch_with_env_override(path: &std::path::Path) -> Option<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(async {
            tokio::time::timeout(BUDGET, fetch(path.as_os_str().to_owned()))
                .await
                .ok()
                .flatten()
        })
    }

    /// A malformed/hostile endpoint claiming a response far larger than
    /// [`MAX_RESPONSE_LEN`] must be rejected before this client allocates a
    /// buffer of that size — never hangs waiting for bytes that never
    /// arrive, and never returns a notice.
    #[tokio::test]
    async fn rejects_a_response_claiming_an_oversized_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Consume the client's request so it isn't left write-blocked,
            // then claim a response far past MAX_RESPONSE_LEN and stop —
            // deliberately never sending that many bytes.
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut discard = vec![0u8; len];
            stream.read_exact(&mut discard).await.unwrap();
            let huge_len = MAX_RESPONSE_LEN + 1;
            stream.write_all(&huge_len.to_be_bytes()).await.unwrap();
        });

        let notice = fetch(path.into_os_string()).await;
        assert_eq!(notice, None);
        server.abort();
    }
}
