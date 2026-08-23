//! Per-session Unix socket for `launch` (#1480): lets a background task
//! (drift watch, credential watch) deliver a one-line notice to the next
//! `hook_run` invocation the engine spawns, without `launch` owning the
//! child's stdio. See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Longest request this server accepts — generous for a fixed one-verb
/// protocol, and small enough that a malformed/hostile client can't make the
/// server allocate an unbounded buffer.
const MAX_REQUEST_LEN: u32 = 4096;

/// How many random bytes back [`LaunchToken::generate`]'s secret. 32 bytes
/// (256 bits) is the conventional size for a bearer token meant to resist
/// guessing, with headroom to spare against any realistic offline search.
const TOKEN_BYTES: usize = 32;

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
/// # Known limitation
/// On Linux, `/proc/<pid>/environ` is readable by the same uid by default, so
/// a same-uid attacker who knows or enumerates `launch`'s pid could still
/// read this token that way. This raises the bar — it requires locating and
/// reading the right pid's environ — compared to no token at all, but it is
/// not a hard guarantee, and callers must not describe it as one.
#[derive(Clone)]
pub(crate) struct LaunchToken(zeroize::Zeroizing<String>);

impl LaunchToken {
    /// Generates a random token: [`TOKEN_BYTES`] bytes from the OS CSPRNG,
    /// hex-encoded so it round-trips cleanly through an env var and JSON.
    ///
    /// # Errors
    /// Returns an error if the OS CSPRNG is unavailable.
    fn generate() -> anyhow::Result<Self> {
        let mut bytes = zeroize::Zeroizing::new([0u8; TOKEN_BYTES]);
        getrandom::fill(bytes.as_mut()).context("generating launch socket token")?;
        Ok(Self(zeroize::Zeroizing::new(hex::encode(*bytes))))
    }

    /// The token's wire form, for setting `LLMENV_LAUNCH_TOKEN` on the
    /// supervised engine's environment.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `candidate` is this token, compared in constant time. A plain
    /// `==` exits at the first mismatched byte, which an attacker probing the
    /// socket could in principle use to learn the token one byte at a time
    /// through response timing.
    fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = candidate.as_bytes();
        expected.len() == actual.len()
            && expected
                .iter()
                .zip(actual)
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0
    }
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
    let token = LaunchToken::generate()?;
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
            if let Err(e) = handle_connection(stream, notices, token).await {
                tracing::debug!("launch: socket connection failed: {e:#}");
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

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(
        len <= MAX_REQUEST_LEN,
        "launch socket request too large: {len} bytes"
    );
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    let request: Request = serde_json::from_slice(&buf)?;

    if !token.matches(&request.token) {
        // Same treatment as a uid mismatch: an expected occurrence (a stale
        // or forged token), not a malfunction, and the client already reads
        // "connection closed with no response" as "no notice" either way.
        tracing::debug!("launch: rejecting socket request with an invalid token");
        return Ok(());
    }

    let response = match request.verb.as_str() {
        "pending_events" => {
            let mut slot = notices.lock().await;
            Response {
                notice: slot.take(),
            }
        }
        other => anyhow::bail!("unknown launch socket verb: {other}"),
    };

    let payload = serde_json::to_vec(&response)?;
    let len: u32 = payload
        .len()
        .try_into()
        .context("launch socket response too large")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    verb: String,
    /// The shared secret from [`LaunchToken`], proving this request came from
    /// a process that inherited it from `launch`'s own environment (#1484).
    token: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Response {
    notice: Option<String>,
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

    /// #1484: a request with a wrong token must be rejected before it can
    /// read the pending notice, even though the peer uid matches. Like a uid
    /// mismatch, rejection closes the connection without writing a response
    /// — so this asserts against that directly rather than through [`fetch`],
    /// which assumes a response always arrives.
    #[tokio::test]
    async fn pending_events_rejects_a_request_with_the_wrong_token() {
        let (listener, notices, path, token) = bind(std::process::id() + 3).unwrap();
        *notices.lock().await = Some("config changed".to_string());
        let server = tokio::spawn(serve(listener, notices, token));

        let mut stream = UnixStream::connect(&path).await.unwrap();
        let request = serde_json::to_vec(&Request {
            verb: "pending_events".to_string(),
            token: "0".repeat(64),
        })
        .unwrap();
        stream
            .write_all(&(request.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&request).await.unwrap();
        let mut len_buf = [0u8; 4];
        assert!(
            stream.read_exact(&mut len_buf).await.is_err(),
            "a wrong token must close the connection without a response"
        );

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    async fn fetch(path: &std::path::Path, token: &str) -> Option<String> {
        let mut stream = UnixStream::connect(path).await.unwrap();
        let request = serde_json::to_vec(&Request {
            verb: "pending_events".to_string(),
            token: token.to_string(),
        })
        .unwrap();
        stream
            .write_all(&(request.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&request).await.unwrap();
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
        let response: Response = serde_json::from_slice(&buf).unwrap();
        response.notice
    }
}
