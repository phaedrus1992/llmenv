//! Per-session Unix socket for `launch` (#1480): lets a background task
//! (drift watch, credential watch) deliver a one-line notice to the next
//! `hook_run` invocation the engine spawns, without `launch` owning the
//! child's stdio. See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Longest request this server accepts — generous for a fixed one-verb
/// protocol, and small enough that a malformed/hostile client can't make the
/// server allocate an unbounded buffer.
const MAX_REQUEST_LEN: u32 = 4096;

/// Shared mailbox: `None` means nothing pending. A background task sets
/// `Some(text)`; the socket server takes it (clearing back to `None`) the
/// first time a client asks — exactly-once delivery.
pub(crate) type NoticeSlot = Arc<Mutex<Option<String>>>;

/// Path for this `launch` invocation's per-session socket. `pid` is
/// `launch`'s own pid, so the path is unique per session by construction.
///
/// # Errors
/// Returns an error when neither `XDG_RUNTIME_DIR` nor llmenv's state dir can
/// be resolved, or the directory can't be created.
pub(crate) fn socket_path(pid: u32) -> anyhow::Result<PathBuf> {
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
        Some(d) if !d.is_empty() => PathBuf::from(d).join("llmenv"),
        _ => crate::paths::state_dir()?,
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("launch-{pid}.sock")))
}

/// Bind the per-session socket, returning the listener, the notice mailbox
/// background tasks push into, and the bound path (for
/// `LLMENV_LAUNCH_SOCKET` and later cleanup).
///
/// # Errors
/// Returns an error when the path can't be resolved or the bind fails.
pub(crate) fn bind(pid: u32) -> anyhow::Result<(UnixListener, NoticeSlot, PathBuf)> {
    let path = socket_path(pid)?;
    // A stale file at this exact path would only exist if this pid was
    // reused since a prior `launch` crashed without tearing down — remove it
    // first so `bind` doesn't fail with "address in use".
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding launch socket at {}", path.display()))?;
    Ok((listener, Arc::new(Mutex::new(None)), path))
}

/// Accept connections until the caller drops this future (i.e. when
/// `launch`'s own supervision loop exits and stops polling it). Each
/// connection is handled on its own spawned task so one slow/malformed
/// client can't block the next.
pub(crate) async fn serve(listener: UnixListener, notices: NoticeSlot) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("launch: socket accept failed: {e:#}");
                continue;
            }
        };
        let notices = Arc::clone(&notices);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, notices).await {
                tracing::debug!("launch: socket connection failed: {e:#}");
            }
        });
    }
}

async fn handle_connection(mut stream: UnixStream, notices: NoticeSlot) -> anyhow::Result<()> {
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
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path_in(Some(dir.path().as_os_str().to_owned()), 12345).unwrap();
        assert_eq!(path, dir.path().join("llmenv").join("launch-12345.sock"));
    }

    #[tokio::test]
    async fn pending_events_delivers_a_queued_notice_exactly_once() {
        let (listener, notices, path) = bind(std::process::id()).unwrap();
        *notices.lock().await = Some("config changed".to_string());
        let server = tokio::spawn(serve(listener, notices));

        let first = fetch(&path).await;
        assert_eq!(first, Some("config changed".to_string()));

        let second = fetch(&path).await;
        assert_eq!(second, None, "a notice must not be delivered twice");

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    async fn fetch(path: &std::path::Path) -> Option<String> {
        let mut stream = UnixStream::connect(path).await.unwrap();
        let request = serde_json::to_vec(&Request {
            verb: "pending_events".to_string(),
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
