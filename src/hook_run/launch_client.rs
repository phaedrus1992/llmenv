//! Client side of `launch`'s per-session socket (#1480): checks for a
//! pending mid-session notice (config drift, credential expiry) on every
//! `hook_run` invocation. See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.
//!
//! The wire *types* (`ClientHello`/`ServerHello`/`Request`/`Response`) are
//! duplicated here deliberately rather than shared as library types with
//! `crate::launch::socket` — the two sides only need to agree on small JSON
//! shapes, and sharing the types would pull `launch` into `hook_run`'s
//! dependency graph for no real benefit. The actual cryptographic and
//! framing primitives (`hmac_hex`, `verify_hmac_hex`, `generate_nonce_hex`,
//! `read_framed`, `write_framed`, `is_authorized_peer`) are *not*
//! duplicated — both sides already call into `launch::socket` for those, so
//! a fix to one side's math can't silently drift from the other's.
//!
//! # Handshake (#1487)
//! Neither side ever puts [`LaunchToken`](crate::launch::socket) on the wire
//! in the clear:
//! 1. This client sends a random nonce.
//! 2. The server must prove it holds the token by returning
//!    `HMAC(token, client_nonce)`, alongside a nonce of its own. This
//!    client verifies that proof — using its own copy of the token from
//!    `LLMENV_LAUNCH_TOKEN` — before doing anything else; a socket pointed
//!    at the wrong endpoint (e.g. via a poisoned `LLMENV_LAUNCH_SOCKET`)
//!    fails right here, before this client has revealed anything that
//!    depends on it holding the token.
//! 3. Only then does this client prove it holds the token in turn, sending
//!    `HMAC(token, server_nonce)` as the request's proof.

use std::time::Duration;

use tokio::net::UnixStream;

use crate::launch::socket::{
    generate_nonce_hex, hmac_hex, is_authorized_peer, read_framed, verify_hmac_hex, write_framed,
};

/// Budget for the whole connect-handshake-request-response round trip,
/// matching the v1 `launch` design's connect-then-fall-back guess. Two
/// extra local round trips for the handshake (#1487) are negligible next to
/// this budget on a Unix domain socket.
const BUDGET: Duration = Duration::from_millis(50);

/// Longest message this client accepts, matching `launch::socket`'s own
/// `MAX_MESSAGE_LEN` — a malformed or hostile endpoint claiming a larger
/// length must not make this client allocate on its say-so.
const MAX_MESSAGE_LEN: u32 = 4096;

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

/// First message of the handshake (#1487), sent by this client.
#[derive(serde::Serialize)]
struct ClientHello {
    nonce: String,
}

/// Second message of the handshake (#1487), sent by the server.
#[derive(serde::Deserialize)]
struct ServerHello {
    proof: String,
    nonce: String,
}

/// Third message (#1487), sent by this client once it has verified
/// [`ServerHello::proof`].
#[derive(serde::Serialize)]
struct Request {
    verb: String,
    proof: String,
}

#[derive(serde::Deserialize)]
struct Response {
    notice: Option<String>,
}

async fn fetch(path: std::ffi::OsString, token: String) -> Option<String> {
    let mut stream = UnixStream::connect(path).await.ok()?;

    // Symmetric to launch::socket's own peer check: refuse to trust a
    // responder running as a different uid, in case whatever bound this
    // path isn't the real `launch` process. Doesn't distinguish a
    // different process at the same uid — the handshake below is what
    // covers that case (#1484, #1487).
    let peer_uid = stream.peer_cred().ok()?.uid();
    let my_uid = rustix::process::geteuid().as_raw();
    if !is_authorized_peer(peer_uid, my_uid) {
        return None;
    }

    let client_nonce = generate_nonce_hex().ok()?;
    write_framed(
        &mut stream,
        &ClientHello {
            nonce: client_nonce.clone(),
        },
    )
    .await
    .ok()?;

    // #1487: verify the responder holds the token *before* proving this
    // client holds it too — a fake endpoint that doesn't know the token
    // can't produce a valid proof here, so this client aborts without ever
    // sending anything that depends on its own copy of the secret.
    let hello: ServerHello = read_framed(&mut stream, MAX_MESSAGE_LEN).await.ok()?;
    if !verify_hmac_hex(&token, client_nonce.as_bytes(), &hello.proof) {
        return None;
    }

    let proof = hmac_hex(&token, hello.nonce.as_bytes()).ok()?;
    write_framed(
        &mut stream,
        &Request {
            verb: "pending_events".to_string(),
            proof,
        },
    )
    .await
    .ok()?;

    let response: Response = read_framed(&mut stream, MAX_MESSAGE_LEN).await.ok()?;
    response.notice
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

    /// #1487: the whole point of the challenge-response — if the responder's
    /// proof doesn't match this client's own copy of the token, this client
    /// must abort right after verifying `ServerHello`, *before* it ever
    /// computes or sends its own proof. Proven with a fake server that sends
    /// a wrong proof and then tries to read a `Request`: if the real client
    /// aborted as intended, that read gets nothing (the connection closes),
    /// not a `Request` a malicious endpoint could otherwise have collected.
    ///
    /// (Length-boundary enforcement on each framed message is covered by
    /// `launch::socket`'s own `read_framed_*` tests, which exercise the
    /// shared framing helper directly rather than through this protocol.)
    #[tokio::test]
    async fn fetch_aborts_before_sending_its_own_proof_when_the_servers_proof_is_wrong() {
        use tokio::io::AsyncReadExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-server.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _hello: serde_json::Value =
                read_framed(&mut stream, MAX_MESSAGE_LEN).await.unwrap();
            write_framed(
                &mut stream,
                &serde_json::json!({ "proof": "0".repeat(64), "nonce": "server-nonce" }),
            )
            .await
            .unwrap();

            // If the real client correctly aborted after rejecting this
            // bogus proof, nothing more ever arrives on this stream.
            let mut buf = [0u8; 1];
            stream.read(&mut buf).await
        });

        let notice = fetch(path.into_os_string(), "real-token".to_string()).await;
        assert_eq!(
            notice, None,
            "a fetch against a server with an invalid proof must return None"
        );

        let read_result = server.await.unwrap();
        assert!(
            matches!(read_result, Ok(0) | Err(_)),
            "the client must not send anything after rejecting the server's proof"
        );
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
