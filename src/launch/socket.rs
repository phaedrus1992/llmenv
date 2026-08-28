//! Per-session Unix socket for `launch` (#1480): lets a background task
//! (drift watch, credential watch) deliver a one-line notice to the next
//! `hook_run` invocation the engine spawns, without `launch` owning the
//! child's stdio. See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

/// Longest message either side of the handshake accepts — generous for a
/// small, fixed-shape protocol, and small enough that a malformed/hostile
/// peer can't make the reader allocate an unbounded buffer.
const MAX_MESSAGE_LEN: u32 = 4096;

/// How many random bytes back [`LaunchToken::generate`]'s secret. 32 bytes
/// (256 bits) is the conventional size for a bearer token meant to resist
/// guessing, with headroom to spare against any realistic offline search.
const TOKEN_BYTES: usize = 32;

/// How many random bytes back each handshake challenge nonce (#1487). 16
/// bytes (128 bits) is ample for a value that only needs to be unpredictable
/// for the lifetime of one connection, never reused or checked against
/// history — a much lighter bar than [`TOKEN_BYTES`]'s long-lived secret.
const NONCE_BYTES: usize = 16;

/// How long [`handle_connection`] waits for a connected peer to complete the
/// whole handshake-and-request exchange before giving up on it. A peer that
/// connects and then stalls (deliberately, or just a bug) would otherwise
/// park a spawned task and its file descriptor forever — a few hundred
/// milliseconds is generous for a local Unix socket, whose client side
/// already budgets 50ms (`hook_run::launch_client::BUDGET`) for the entire
/// round trip.
const CONNECTION_TIMEOUT: Duration = Duration::from_millis(500);

/// Shared mailbox: `None` means nothing pending. A background task sets
/// `Some(text)`; the socket server takes it (clearing back to `None`) the
/// first time a client asks — exactly-once delivery.
pub(crate) type NoticeSlot = Arc<Mutex<Option<String>>>;

/// Per-session shared secret (#1484) that lets [`handle_connection`] reject
/// any process that didn't inherit it from `launch`'s own environment,
/// closing the gap a uid check alone leaves open: a different process
/// running as the *same* uid as `launch` still passes
/// `peer_uid == my_uid` in [`is_authorized_peer`], so that check alone cannot
/// tell a compromised same-uid dependency from the real engine's descendant.
///
/// Neither side ever puts this secret on the wire in the clear (#1487): both
/// prove knowledge of it via an HMAC challenge-response (see
/// [`hmac_hex`]/[`verify_hmac_hex`] and [`handle_connection`]), so a process
/// pointed at the wrong socket path — say, via a poisoned
/// `LLMENV_LAUNCH_SOCKET` — can't harvest the token merely by getting a
/// client to connect to it.
///
/// # Known limitations
/// - The token travels as an env var, which every descendant of the
///   supervised engine inherits, not just `hook_run`'s own invocations — a
///   third-party MCP server, a hook, or any other command the engine runs
///   can read it from its own environment. This is inherent to the
///   env-inheritance transport, not a bug: it is the same population of
///   processes the token is meant to admit (anything spawned from the
///   engine's own session), and there is no cheaper way to reach every
///   `hook_run` invocation without it.
/// - On Linux, `/proc/<pid>/environ` is readable by the same uid by default,
///   so a same-uid attacker who knows or enumerates `launch`'s pid could
///   still read this token that way. This raises the bar — it requires
///   locating and reading the right pid's environ — compared to no token at
///   all, but it is not a hard guarantee, and callers must not describe it
///   as one.
#[derive(Clone)]
pub(crate) struct LaunchToken(zeroize::Zeroizing<String>);

impl LaunchToken {
    /// Generates a random token: [`TOKEN_BYTES`] bytes from the OS CSPRNG,
    /// hex-encoded so it round-trips cleanly through an env var and JSON.
    /// `pub(crate)` (not just used by this module's own [`bind`]) since the
    /// launch proxy (#1632) reuses this same generator for its own,
    /// independent peer-auth secret rather than duplicating it.
    ///
    /// # Errors
    /// Returns an error if the OS CSPRNG is unavailable.
    pub(crate) fn generate() -> anyhow::Result<Self> {
        let mut bytes = zeroize::Zeroizing::new([0u8; TOKEN_BYTES]);
        getrandom::fill(bytes.as_mut()).context("generating launch socket token")?;
        Ok(Self(zeroize::Zeroizing::new(hex::encode(*bytes))))
    }

    /// The token's wire form, for setting `LLMENV_LAUNCH_TOKEN` on the
    /// supervised engine's environment.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `candidate` has the exact shape [`LaunchToken::generate`]
/// produces: [`TOKEN_BYTES`] `* 2` hex characters. `HmacSha256::new_from_slice`
/// accepts a key of *any* length, including an empty string, so without this
/// check a truncated or malformed `LLMENV_LAUNCH_TOKEN` would silently
/// become a valid — if wrong — HMAC key rather than being rejected outright
/// (#1487). This is a shape check only, not a defense against a caller who
/// controls `LLMENV_LAUNCH_TOKEN` itself: a process able to set that env var
/// to a value of its own choosing can always satisfy this check trivially,
/// same as it could satisfy the crypto itself.
pub(crate) fn is_well_formed_token(candidate: &str) -> bool {
    candidate.len() == TOKEN_BYTES * 2 && hex::decode(candidate).is_ok()
}

/// Computes HMAC-SHA256 keyed by `secret` over `message`, hex-encoded for the
/// wire. The shared primitive behind the handshake's challenge-response
/// (#1487): both `launch` (via [`LaunchToken::as_str`]) and `hook_run` (via
/// its own copy of `LLMENV_LAUNCH_TOKEN`) call this with the same secret, so
/// it takes a plain `&str` rather than a [`LaunchToken`] — the client side
/// never constructs one.
///
/// # Errors
/// HMAC-SHA256 accepts a key of any length, so this only fails if that
/// invariant is ever violated upstream; the `Result` keeps the interface
/// honest rather than asserting infallibility outright.
pub(crate) fn hmac_hex(secret: &str, message: &[u8]) -> anyhow::Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .context("constructing HMAC for launch socket handshake")?;
    mac.update(message);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Whether `candidate_hex` is `secret`'s HMAC-SHA256 over `message`,
/// verified in constant time via [`Mac::verify_slice`]. Returns `false` —
/// not an error — for a malformed hex candidate or a mismatch alike: both
/// are an invalid proof, not a malfunction worth propagating.
///
/// `#[must_use]`: this is a security predicate — a call site that computes
/// it and then ignores the result would silently treat every peer as
/// authenticated.
#[must_use]
pub(crate) fn verify_hmac_hex(secret: &str, message: &[u8], candidate_hex: &str) -> bool {
    let Ok(candidate) = hex::decode(candidate_hex) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(message);
    mac.verify_slice(&candidate).is_ok()
}

/// Builds the byte message an HMAC proof binds to (#1487): `role`, then both
/// nonces, then `extra` — each part separated by a NUL the parts themselves
/// can't contain unescaped meaning across (hex nonces, ASCII role labels,
/// and `extra`'s own length-implicit boundary make collisions between two
/// different `(role, client_nonce, server_nonce, extra)` tuples producing
/// the same bytes practically impossible).
///
/// The `role` label is what makes this a *domain-separated* construction:
/// [`ServerHello::proof`] uses `"server"`, [`Request::proof`] uses
/// `"client"`, and the notice's own proof (see `handle_connection`) uses
/// `"response"`. Without this, all three proofs would be
/// `HMAC(token, some nonce)` — and a peer that never learned the token could
/// still win authentication by opening a second connection, echoing the
/// first connection's `server_nonce` back as its *own* `ClientHello.nonce`,
/// and replaying the server's honest reply as the first connection's own
/// request proof (the server, asked to prove itself on connection two, ends
/// up computing exactly the value connection one's request needed — a
/// reflection attack, since both roles shared one undifferentiated message
/// space). Giving each role a distinct label closes that: the server never
/// computes a `"client"`- or `"response"`-labeled value on a peer-supplied
/// input, only `"server"`-labeled ones, so there is no way to turn the
/// server into an oracle for a proof it would otherwise demand.
pub(crate) fn handshake_message(
    role: &str,
    client_nonce: &str,
    server_nonce: &str,
    extra: &[u8],
) -> Vec<u8> {
    let mut message = Vec::with_capacity(
        role.len() + 1 + client_nonce.len() + 1 + server_nonce.len() + 1 + extra.len(),
    );
    message.extend_from_slice(role.as_bytes());
    message.push(0);
    message.extend_from_slice(client_nonce.as_bytes());
    message.push(0);
    message.extend_from_slice(server_nonce.as_bytes());
    message.push(0);
    message.extend_from_slice(extra);
    message
}

/// Encodes a [`Response::notice`] as the `extra` bytes for its own proof
/// (#1487): a discriminant byte (`0` for `None`, `1` for `Some`) followed by
/// the text, if any — without the discriminant, `None` and `Some(String::new())`
/// would hash identically, which is harmless here (both mean "nothing to
/// show") but is exactly the kind of ambiguity a MAC's input encoding should
/// not leave to chance.
pub(crate) fn notice_bytes(notice: &Option<String>) -> Vec<u8> {
    match notice {
        Some(text) => std::iter::once(1).chain(text.bytes()).collect(),
        None => vec![0],
    }
}

/// Generates a random handshake challenge nonce (#1487): [`NONCE_BYTES`]
/// bytes from the OS CSPRNG, hex-encoded so it round-trips through JSON like
/// [`LaunchToken`]'s own wire form. Shared by both ends of the socket —
/// `launch` calls this for its own nonce in [`handle_connection`], and
/// `hook_run`'s client calls it for its `ClientHello` nonce.
///
/// # Errors
/// Returns an error if the OS CSPRNG is unavailable.
pub(crate) fn generate_nonce_hex() -> anyhow::Result<String> {
    let mut bytes = [0u8; NONCE_BYTES];
    getrandom::fill(&mut bytes).context("generating launch socket handshake nonce")?;
    Ok(hex::encode(bytes))
}

/// Reads one length-prefixed JSON message from `stream`: a 4-byte
/// big-endian length header followed by that many bytes of JSON. Shared
/// framing for every message either side of the socket sends — the framing
/// itself carries no protocol-specific meaning, so it doesn't need
/// per-message duplication the way the JSON shapes deliberately do (see this
/// module's sibling `hook_run::launch_client`).
///
/// # Errors
/// Returns an error on any I/O failure, if the declared length exceeds
/// `max_len`, or if the bytes read don't parse as `T`.
pub(crate) async fn read_framed<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
    max_len: u32,
) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(
        len <= max_len,
        "launch socket message too large: {len} bytes"
    );
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

/// Writes one length-prefixed JSON message to `stream` — the write side of
/// [`read_framed`].
///
/// # Errors
/// Returns an error on any I/O failure, or if `value` serializes to more
/// bytes than a `u32` can express as a length header.
pub(crate) async fn write_framed<T: serde::Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(value)?;
    let len: u32 = payload
        .len()
        .try_into()
        .context("launch socket message too large to encode its length")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

impl std::fmt::Debug for LaunchToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaunchToken(<redacted>)")
    }
}

/// Path for this `launch` invocation's per-session socket. `pid` is
/// `launch`'s own pid, so the path is unique per session by construction.
///
/// # Errors
/// Returns an error when neither `XDG_RUNTIME_DIR` nor llmenv's state dir can
/// be resolved, or the directory can't be created.
fn socket_path(pid: u32) -> anyhow::Result<PathBuf> {
    socket_path_in(std::env::var_os("XDG_RUNTIME_DIR"), pid)
}

/// [`socket_path`] split out so the `XDG_RUNTIME_DIR` value is a parameter —
/// mutating real process env vars from a unit test is `unsafe` under Rust
/// 2024 (forbidden workspace-wide) and races other tests in the same binary
/// (#1305, see `adapter::active_adapter_from`'s doc comment for the same
/// pattern). Test against this directly instead.
fn socket_path_in(
    xdg_runtime_dir: Option<std::ffi::OsString>,
    pid: u32,
) -> anyhow::Result<PathBuf> {
    let dir = match xdg_runtime_dir {
        // A relative value (e.g. `XDG_RUNTIME_DIR=.`) would otherwise resolve
        // against the current working directory — typically a project
        // checkout, not a runtime directory — putting the socket somewhere
        // repo permissions govern instead of the runtime dir's.
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d).join("llmenv"),
        _ => crate::paths::state_dir()?,
    };
    // Owner-only: this socket carries a mid-session notice into the agent's
    // own context, so anything able to connect and answer on it can inject
    // arbitrary text there. `create_dir_owner_only` also self-heals an
    // existing directory left permissive by an older llmenv version or a
    // loose umask.
    crate::paths::create_dir_owner_only(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("launch-{pid}.sock")))
}

/// Bind the per-session socket, returning the listener, the notice mailbox
/// background tasks push into, the bound path (for `LLMENV_LAUNCH_SOCKET` and
/// later cleanup), and the shared secret (for `LLMENV_LAUNCH_TOKEN`, #1484).
///
/// # Errors
/// Returns an error when the path can't be resolved, the bind fails, or the
/// token can't be generated.
pub(crate) fn bind(pid: u32) -> anyhow::Result<(UnixListener, NoticeSlot, PathBuf, LaunchToken)> {
    // Generated before any filesystem state exists: a `UnixListener` doesn't
    // unlink its socket file on drop, so if this failed after `bind` below,
    // the caller would never receive `path` to construct its own cleanup
    // guard from, leaking the socket file on a CSPRNG failure.
    let token = LaunchToken::generate()?;
    let path = socket_path(pid)?;
    // A stale file at this exact path would only exist if this pid was
    // reused since a prior `launch` crashed without tearing down — remove it
    // first so `bind` doesn't fail with "address in use".
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding launch socket at {}", path.display()))?;
    // `bind` creates the socket file at the process umask, which can leave
    // it group/world-readable (0755 under a common 022 umask) even though
    // its parent directory is now owner-only — chmod it explicitly rather
    // than relying on the directory alone.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("hardening permissions on {}", path.display()))?;
    }
    Ok((listener, Arc::new(Mutex::new(None)), path, token))
}

/// Accept connections until the caller drops this future (i.e. when
/// `launch`'s own supervision loop exits and stops polling it). Each
/// connection is handled on its own spawned task so one slow/malformed
/// client can't block the next.
pub(crate) async fn serve(listener: UnixListener, notices: NoticeSlot, token: LaunchToken) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("launch: socket accept failed: {e:#}");
                continue;
            }
        };
        let notices = Arc::clone(&notices);
        let token = token.clone();
        tokio::spawn(async move {
            // Expected rejections (uid mismatch, invalid proof, a stalled
            // peer) already return `Ok(())` and log their own `debug!`
            // inside `handle_connection`/`handle_authorized_connection`. An
            // `Err` here is something else — a malformed frame, a protocol
            // mismatch (unknown verb), or a CSPRNG/HMAC-construction
            // failure — worth `warn!` so it doesn't blend into the routine
            // rejection noise at `debug!`.
            if let Err(e) = handle_connection(stream, notices, token).await {
                tracing::warn!("launch: socket connection failed: {e:#}");
            }
        });
    }
}

/// Whether a connecting peer with `peer_uid` is allowed to talk to this
/// socket — it must be the same principal as this process, `my_uid`. Split
/// out as a pure function so the comparison is unit-testable: the real
/// inputs come from a syscall (`UnixStream::peer_cred`, `geteuid`) that
/// can't be mocked, but the decision they feed can (same
/// split-for-testability pattern as `socket_path_in`).
///
/// This is a first, coarse layer of defense — it stops a different local
/// user from connecting — on top of the socket's directory/file already being
/// owner-only. It cannot by itself stop a different process running as the
/// *same* uid; [`LaunchToken`] is the layer that closes that gap.
pub(crate) fn is_authorized_peer(peer_uid: u32, my_uid: u32) -> bool {
    peer_uid == my_uid
}

/// Handles one connection: the uid check, then the handshake-and-request
/// exchange under [`CONNECTION_TIMEOUT`] (see [`handle_authorized_connection`]
/// for the handshake itself).
async fn handle_connection(
    mut stream: UnixStream,
    notices: NoticeSlot,
    token: LaunchToken,
) -> anyhow::Result<()> {
    let peer_uid = stream.peer_cred()?.uid();
    let my_uid = rustix::process::geteuid().as_raw();
    if !is_authorized_peer(peer_uid, my_uid) {
        // Not an error — a mismatched peer is an expected, if rare, case
        // (another tool running as a different local user probing the
        // socket path), not a malfunction worth `warn!`.
        tracing::debug!("launch: rejecting socket peer with uid {peer_uid} (expected {my_uid})");
        return Ok(());
    }

    match tokio::time::timeout(
        CONNECTION_TIMEOUT,
        handle_authorized_connection(&mut stream, notices, token),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => {
            tracing::debug!("launch: socket connection timed out after {CONNECTION_TIMEOUT:?}");
            Ok(())
        }
    }
}

/// Runs the full handshake (#1487) before serving a request:
///
/// 1. The client sends [`ClientHello`] with a random nonce.
/// 2. This server proves it holds [`LaunchToken`] by returning a
///    `"server"`-labeled proof in [`ServerHello`], alongside a nonce of its
///    own.
/// 3. The client — which independently verifies that proof before ever
///    calling back — proves it holds the token in turn by sending a
///    `"client"`-labeled proof as [`Request::proof`].
/// 4. This server answers with [`Response`], itself carrying a
///    `"response"`-labeled proof over the notice content, so a relay that
///    faithfully forwards steps 1-3 (without ever learning the token) still
///    can't substitute its own text for the real notice in step 4 — the
///    three roles are domain-separated by [`handshake_message`] precisely
///    so none of them can be produced by asking either party to compute a
///    *different* one (see that function's doc for the reflection attack
///    this closes).
///
/// The raw token never appears on the wire in any direction; only these
/// one-way proofs of knowledge do, which is what closes the gap #1484's
/// plain bearer token left open (a process pointed at the wrong socket path
/// could harvest that raw token on first connection).
async fn handle_authorized_connection(
    stream: &mut UnixStream,
    notices: NoticeSlot,
    token: LaunchToken,
) -> anyhow::Result<()> {
    let hello: ClientHello = read_framed(stream, MAX_MESSAGE_LEN).await?;
    let server_nonce = generate_nonce_hex()?;
    let server_proof = hmac_hex(
        token.as_str(),
        &handshake_message("server", &hello.nonce, &server_nonce, &[]),
    )?;
    write_framed(
        stream,
        &ServerHello {
            proof: server_proof,
            nonce: server_nonce.clone(),
        },
    )
    .await?;

    let request: Request = read_framed(stream, MAX_MESSAGE_LEN).await?;
    let expected_request_message = handshake_message("client", &hello.nonce, &server_nonce, &[]);
    if !verify_hmac_hex(token.as_str(), &expected_request_message, &request.proof) {
        // Same treatment as a uid mismatch: an expected occurrence (a stale
        // or forged proof), not a malfunction, and the client already reads
        // "connection closed with no response" as "no notice" either way.
        tracing::debug!("launch: rejecting socket request with an invalid proof");
        return Ok(());
    }

    let notice = match request.verb.as_str() {
        "pending_events" => {
            let mut slot = notices.lock().await;
            slot.take()
        }
        other => anyhow::bail!("unknown launch socket verb: {other}"),
    };
    let response_message = handshake_message(
        "response",
        &hello.nonce,
        &server_nonce,
        &notice_bytes(&notice),
    );
    let response_proof = hmac_hex(token.as_str(), &response_message)?;

    write_framed(
        stream,
        &Response {
            notice,
            proof: response_proof,
        },
    )
    .await
}

/// First message of the handshake (#1487), client → server.
#[derive(serde::Serialize, serde::Deserialize)]
struct ClientHello {
    /// Random per-connection challenge. The server must return its
    /// HMAC-SHA256 over this value, keyed by [`LaunchToken`], before this
    /// client will send anything that depends on it also holding the token.
    nonce: String,
}

/// Second message of the handshake (#1487), server → client.
#[derive(serde::Serialize, serde::Deserialize)]
struct ServerHello {
    /// The `"server"`-role [`handshake_message`], HMAC'd with
    /// [`LaunchToken`] and hex-encoded — proof the server holds it, checked
    /// by the client before it proceeds.
    proof: String,
    /// The server's own random nonce, folded into every proof below so a
    /// captured [`Request`] or [`Response`] can't be replayed against a
    /// different connection.
    nonce: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    verb: String,
    /// The `"client"`-role [`handshake_message`], HMAC'd with
    /// [`LaunchToken`] and hex-encoded — proves this request came from a
    /// process that holds the token, without ever transmitting it (#1487).
    proof: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Response {
    notice: Option<String>,
    /// The `"response"`-role [`handshake_message`] (bound to `notice` via
    /// [`notice_bytes`]), HMAC'd with [`LaunchToken`] and hex-encoded —
    /// proves this specific notice content, not just the earlier
    /// handshake, came from a process that holds the token. Without this, a
    /// relay that faithfully forwards `ClientHello`/`ServerHello`/`Request`
    /// (never learning the token itself) could still substitute arbitrary
    /// text here, since authenticating the handshake alone says nothing
    /// about what's sent after it (#1487).
    proof: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn is_authorized_peer_true_for_matching_uid() {
        assert!(is_authorized_peer(1000, 1000));
    }

    #[test]
    fn is_authorized_peer_false_for_mismatched_uid() {
        assert!(!is_authorized_peer(1000, 1001));
    }

    #[test]
    fn generate_produces_a_valid_hex_token_of_the_expected_length() {
        let token = LaunchToken::generate().unwrap();
        let decoded = hex::decode(token.as_str()).expect("token must be valid hex");
        assert_eq!(decoded.len(), TOKEN_BYTES);
    }

    #[test]
    fn generate_produces_distinct_tokens() {
        let a = LaunchToken::generate().unwrap();
        let b = LaunchToken::generate().unwrap();
        assert_ne!(
            a.as_str(),
            b.as_str(),
            "two generated tokens must not collide"
        );
    }

    use proptest::prelude::*;

    proptest::proptest! {
        /// #1487: `verify_hmac_hex` must accept exactly the proof `hmac_hex`
        /// computed for the same secret and message, and reject a proof
        /// computed under a different secret — the property the whole
        /// challenge-response leans on.
        #[test]
        fn verify_hmac_hex_accepts_its_own_proof_and_rejects_a_different_secret(
            secret_a in "[0-9a-f]{64}", secret_b in "[0-9a-f]{64}",
            message in "\\PC{0,100}",
        ) {
            let proof_a = hmac_hex(&secret_a, message.as_bytes()).unwrap();
            prop_assert!(verify_hmac_hex(&secret_a, message.as_bytes(), &proof_a));
            if secret_a != secret_b {
                prop_assert!(!verify_hmac_hex(&secret_b, message.as_bytes(), &proof_a));
            }
        }

        /// Same property, but varying the message an otherwise-fixed secret
        /// is asked to prove knowledge over: a proof computed for one nonce
        /// must not verify against a different one, which is what stops a
        /// captured [`Request`] from one connection being replayed into
        /// another.
        #[test]
        fn verify_hmac_hex_rejects_a_proof_computed_for_a_different_message(
            secret in "[0-9a-f]{64}", message_a in "\\PC{1,50}", message_b in "\\PC{1,50}",
        ) {
            let proof = hmac_hex(&secret, message_a.as_bytes()).unwrap();
            if message_a != message_b {
                prop_assert!(!verify_hmac_hex(&secret, message_b.as_bytes(), &proof));
            }
        }

        /// The wire `Request` (verb + proof) must round-trip arbitrary
        /// content through JSON byte-for-byte, independent of what the verb
        /// or proof actually contain.
        #[test]
        fn request_roundtrips_arbitrary_verb_and_proof(
            verb in "\\PC{0,50}", proof in "\\PC{0,100}",
        ) {
            let request = Request { verb: verb.clone(), proof: proof.clone() };
            let bytes = serde_json::to_vec(&request).unwrap();
            let decoded: Request = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(decoded.verb, verb);
            prop_assert_eq!(decoded.proof, proof);
        }

        /// [`ClientHello`] and [`ServerHello`] must round-trip the same way.
        #[test]
        fn hello_messages_roundtrip_arbitrary_content(
            nonce in "\\PC{0,100}", proof in "\\PC{0,100}", server_nonce in "\\PC{0,100}",
        ) {
            let client_hello = ClientHello { nonce: nonce.clone() };
            let bytes = serde_json::to_vec(&client_hello).unwrap();
            let decoded: ClientHello = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(decoded.nonce, nonce);

            let server_hello = ServerHello { proof: proof.clone(), nonce: server_nonce.clone() };
            let bytes = serde_json::to_vec(&server_hello).unwrap();
            let decoded: ServerHello = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(decoded.proof, proof);
            prop_assert_eq!(decoded.nonce, server_nonce);
        }

        /// [`Response`] must round-trip the same way, including an absent
        /// notice.
        #[test]
        fn response_roundtrips_arbitrary_content(
            notice in proptest::option::of("\\PC{0,100}"), proof in "\\PC{0,100}",
        ) {
            let response = Response { notice: notice.clone(), proof: proof.clone() };
            let bytes = serde_json::to_vec(&response).unwrap();
            let decoded: Response = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(decoded.notice, notice);
            prop_assert_eq!(decoded.proof, proof);
        }

        /// #1487: `verify_hmac_hex` must never panic on a candidate that
        /// isn't valid hex — a wire value from an untrusted peer, which
        /// [`hex::decode`] is only ever asked to attempt, never assumed to
        /// succeed.
        #[test]
        fn verify_hmac_hex_never_panics_on_malformed_hex(
            secret in "[0-9a-f]{64}", message in "\\PC{0,50}", candidate in "\\PC{0,200}",
        ) {
            let _ = verify_hmac_hex(&secret, message.as_bytes(), &candidate);
        }

        /// [`generate_nonce_hex`] must always produce valid hex of exactly
        /// [`NONCE_BYTES`] * 2 characters — the shape both sides of the
        /// handshake assume when treating a peer's nonce as an opaque wire
        /// value.
        #[test]
        fn generate_nonce_hex_always_produces_valid_hex_of_expected_length(_unit in Just(())) {
            let nonce = generate_nonce_hex().unwrap();
            prop_assert_eq!(nonce.len(), NONCE_BYTES * 2);
            prop_assert!(hex::decode(&nonce).is_ok());
        }

        /// #1487: the whole point of [`handshake_message`] — the same
        /// `(client_nonce, server_nonce)` pair must produce a different
        /// message for each role, which is what closes the reflection
        /// attack `pending_events_rejects_the_two_connection_reflection_attack`
        /// exercises end to end (the server never computes a message under
        /// a role it wasn't asked to prove, so there is no oracle for the
        /// role it will later verify).
        #[test]
        fn handshake_message_differs_by_role_for_the_same_nonces(
            client_nonce in "\\PC{1,50}", server_nonce in "\\PC{1,50}",
        ) {
            let server_message = handshake_message("server", &client_nonce, &server_nonce, &[]);
            let client_message = handshake_message("client", &client_nonce, &server_nonce, &[]);
            let response_message = handshake_message("response", &client_nonce, &server_nonce, &[]);
            prop_assert_ne!(&server_message, &client_message);
            prop_assert_ne!(&server_message, &response_message);
            prop_assert_ne!(&client_message, &response_message);
        }

        /// [`read_framed`]/[`write_framed`] must round-trip an arbitrary
        /// [`ClientHello`] byte-for-byte through a real socket, not just
        /// prove `serde_json` itself round-trips (already covered by
        /// `hello_messages_roundtrip_arbitrary_content`) — this is the pair
        /// property-test-gap-finder asked for on the framing primitive
        /// itself, independent of any one wire type.
        #[test]
        fn read_framed_write_framed_roundtrip_arbitrary_client_hello(nonce in "\\PC{0,200}") {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let received_nonce = rt.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("roundtrip.sock");
                let listener = UnixListener::bind(&path).unwrap();
                let hello = ClientHello { nonce: nonce.clone() };

                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let received: ClientHello = read_framed(&mut stream, MAX_MESSAGE_LEN).await.unwrap();
                    received.nonce
                });

                let mut client = UnixStream::connect(&path).await.unwrap();
                write_framed(&mut client, &hello).await.unwrap();
                server.await.unwrap()
            });
            prop_assert_eq!(received_nonce, nonce);
        }
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path_in(Some(dir.path().as_os_str().to_owned()), 12345).unwrap();
        assert_eq!(path, dir.path().join("llmenv").join("launch-12345.sock"));
    }

    #[test]
    fn socket_path_falls_back_to_state_dir_when_xdg_runtime_dir_is_relative() {
        // A relative XDG_RUNTIME_DIR must not be honored — it would resolve
        // against the current working directory (typically a project
        // checkout) rather than a real runtime dir. Can't assert the exact
        // fallback path without controlling `state_dir()`'s own env inputs
        // (unsafe, forbidden), so assert the negative: the relative value
        // is not the one used.
        let path = socket_path_in(Some("relative/runtime/dir".into()), 12345).unwrap();
        assert!(
            !path.starts_with("relative/runtime/dir"),
            "a relative XDG_RUNTIME_DIR must not be honored; got {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn bind_creates_an_owner_only_directory_and_socket() {
        use std::os::unix::fs::PermissionsExt;

        // Offset from the other tests' pids in this binary (plain
        // `std::process::id()`, `+1` in `hook_run::launch_client`'s tests)
        // so a parallel run doesn't have two tests binding the same path.
        let (_listener, _notices, path, _token) = bind(std::process::id() + 2).unwrap();

        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "socket directory must be owner-only");

        let socket_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(socket_mode, 0o600, "socket file must be owner-only");

        let _ = std::fs::remove_file(&path);
    }

    /// Also the coverage for the peer-uid check accepting a legitimate
    /// connection: `peer_cred()` reports the connecting process's real
    /// uid, which is the same value regardless of whether the connection
    /// comes from another task in this test binary or a genuinely separate
    /// process — both run as this test's own uid, the case `is_authorized_peer`
    /// must accept. A mismatched-uid rejection can't be integration-tested
    /// without a second real uid, which CI doesn't provide; that path is
    /// covered by `is_authorized_peer_false_for_mismatched_uid` instead.
    #[tokio::test]
    async fn pending_events_delivers_a_queued_notice_exactly_once() {
        let (listener, notices, path, token) = bind(std::process::id()).unwrap();
        *notices.lock().await = Some("config changed".to_string());
        let server = tokio::spawn(serve(listener, notices, token.clone()));

        let first = fetch(&path, token.as_str()).await;
        assert_eq!(first, Some("config changed".to_string()));

        let second = fetch(&path, token.as_str()).await;
        assert_eq!(second, None, "a notice must not be delivered twice");

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// #1487: a request with a proof computed from the wrong token must be
    /// rejected before it can read the pending notice, even though the peer
    /// uid matches and the handshake's first two messages complete
    /// normally. Like a uid mismatch, rejection closes the connection
    /// without writing a response — so this asserts against that directly
    /// rather than through [`fetch`], which assumes a response always
    /// arrives.
    #[tokio::test]
    async fn pending_events_rejects_a_request_with_an_invalid_proof() {
        let (listener, notices, path, token) = bind(std::process::id() + 3).unwrap();
        *notices.lock().await = Some("config changed".to_string());
        let server = tokio::spawn(serve(listener, notices, token));

        let mut stream = UnixStream::connect(&path).await.unwrap();
        write_framed(
            &mut stream,
            &ClientHello {
                nonce: "client-nonce".to_string(),
            },
        )
        .await
        .unwrap();
        let _hello: ServerHello = read_framed(&mut stream, MAX_MESSAGE_LEN).await.unwrap();

        write_framed(
            &mut stream,
            &Request {
                verb: "pending_events".to_string(),
                proof: "0".repeat(64),
            },
        )
        .await
        .unwrap();
        let mut len_buf = [0u8; 4];
        assert!(
            stream.read_exact(&mut len_buf).await.is_err(),
            "an invalid proof must close the connection without a response"
        );

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_framed_accepts_a_message_of_exactly_the_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let payload = padded_json_payload_of_exact_len(MAX_MESSAGE_LEN);

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(&(payload.len() as u32).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&payload).await.unwrap();
        });

        let mut stream = UnixStream::connect(&path).await.unwrap();
        let result: Result<serde_json::Value, _> = read_framed(&mut stream, MAX_MESSAGE_LEN).await;
        assert!(
            result.is_ok(),
            "an exactly-max-length message must not be rejected"
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_framed_rejects_a_message_one_byte_past_the_max_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let payload = padded_json_payload_of_exact_len(MAX_MESSAGE_LEN + 1);

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(&(payload.len() as u32).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&payload).await.unwrap();
        });

        let mut stream = UnixStream::connect(&path).await.unwrap();
        let result: Result<serde_json::Value, _> = read_framed(&mut stream, MAX_MESSAGE_LEN).await;
        assert!(
            result.is_err(),
            "a message one byte past the max length must be rejected"
        );

        server.await.unwrap();
    }

    /// A valid, complete JSON object `{"pad":"<padding>"}` whose total
    /// serialized length is exactly `len` bytes — built by construction
    /// (fixed prefix/suffix, padding fills the rest) rather than trial and
    /// error, so a boundary test can hit an exact byte count. `len` must be
    /// at least the 9-byte fixed overhead (`{"pad":"` + `"}`).
    fn padded_json_payload_of_exact_len(len: u32) -> Vec<u8> {
        const PREFIX: &[u8] = b"{\"pad\":\"";
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

    /// #1487: a peer that connects and then never sends `ClientHello` must
    /// not park the spawned connection task (and its fd) forever — the
    /// server must close the connection once [`CONNECTION_TIMEOUT`] elapses.
    /// Observed from the client's own side: a `read` that returns `Ok(0)`
    /// (EOF) means the server dropped its end of the stream.
    #[tokio::test]
    async fn handle_connection_closes_a_peer_that_never_completes_the_handshake() {
        let (listener, notices, path, token) = bind(std::process::id() + 7).unwrap();
        let server = tokio::spawn(serve(listener, notices, token));

        let mut stream = UnixStream::connect(&path).await.unwrap();
        // Deliberately send nothing — the server is left waiting on
        // ClientHello.

        let mut buf = [0u8; 1];
        let result = tokio::time::timeout(CONNECTION_TIMEOUT * 3, stream.read(&mut buf)).await;
        assert!(
            matches!(result, Ok(Ok(0))),
            "the server must close a stalled connection once CONNECTION_TIMEOUT \
             elapses, got {result:?}"
        );

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// #1487 regression: the reflection attack the initial (undifferentiated)
    /// design was vulnerable to. Opening a second connection and echoing the
    /// first connection's own `server_nonce` back as that second
    /// connection's `ClientHello.nonce` makes the server sign exactly the
    /// value the first connection's `Request.proof` needed — with zero
    /// knowledge of the token — *unless* the two proofs are domain-separated
    /// by role, which [`handshake_message`] does. Proves that replaying the
    /// second connection's `ServerHello.proof` into the first connection's
    /// request is rejected.
    #[tokio::test]
    async fn pending_events_rejects_the_two_connection_reflection_attack() {
        let (listener, notices, path, token) = bind(std::process::id() + 6).unwrap();
        *notices.lock().await = Some("config changed".to_string());
        let server = tokio::spawn(serve(listener, notices, token));

        // Connection A: start the handshake, capture its server_nonce.
        let mut stream_a = UnixStream::connect(&path).await.unwrap();
        write_framed(
            &mut stream_a,
            &ClientHello {
                nonce: "attacker-nonce-a".to_string(),
            },
        )
        .await
        .unwrap();
        let hello_a: ServerHello = read_framed(&mut stream_a, MAX_MESSAGE_LEN).await.unwrap();

        // Connection B: echo A's server_nonce as B's own ClientHello nonce,
        // so the server signs exactly the value A's request would need
        // under an undifferentiated (non-domain-separated) construction.
        let mut stream_b = UnixStream::connect(&path).await.unwrap();
        write_framed(
            &mut stream_b,
            &ClientHello {
                nonce: hello_a.nonce.clone(),
            },
        )
        .await
        .unwrap();
        let hello_b: ServerHello = read_framed(&mut stream_b, MAX_MESSAGE_LEN).await.unwrap();

        // Replay B's proof as A's request proof.
        write_framed(
            &mut stream_a,
            &Request {
                verb: "pending_events".to_string(),
                proof: hello_b.proof,
            },
        )
        .await
        .unwrap();
        let mut len_buf = [0u8; 4];
        assert!(
            stream_a.read_exact(&mut len_buf).await.is_err(),
            "the reflected proof must not be accepted as connection A's request proof"
        );

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// Performs the client side of the handshake against a real `launch`
    /// socket server, for tests that only need to drive [`serve`] end to
    /// end. Skips the peer-uid check `hook_run::launch_client::fetch`
    /// performs first — irrelevant here, since both ends of a test always
    /// run as the same uid.
    async fn fetch(path: &std::path::Path, token: &str) -> Option<String> {
        let mut stream = UnixStream::connect(path).await.ok()?;

        let client_nonce = generate_nonce_hex().ok()?;
        write_framed(
            &mut stream,
            &ClientHello {
                nonce: client_nonce.clone(),
            },
        )
        .await
        .ok()?;

        let hello: ServerHello = read_framed(&mut stream, MAX_MESSAGE_LEN).await.ok()?;
        let expected_server_message = handshake_message("server", &client_nonce, &hello.nonce, &[]);
        if !verify_hmac_hex(token, &expected_server_message, &hello.proof) {
            return None;
        }

        let request_message = handshake_message("client", &client_nonce, &hello.nonce, &[]);
        let proof = hmac_hex(token, &request_message).ok()?;
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
        let response_message = handshake_message(
            "response",
            &client_nonce,
            &hello.nonce,
            &notice_bytes(&response.notice),
        );
        if !verify_hmac_hex(token, &response_message, &response.proof) {
            return None;
        }
        response.notice
    }
}
