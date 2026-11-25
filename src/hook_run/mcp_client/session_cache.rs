//! The MCP session id, shared by the hook processes of one memory server (#2909).
//!
//! Each hook is a new process, so an id kept in memory is lost at once, and each process paid an
//! `initialize` round trip before its first call. A file in the state folder hands the id to the
//! next process. The server stays the judge of the id: a rejected id is forgotten, and the call
//! retries once with a new one.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long a saved id is trusted. The server expires its session on its own clock, and a
/// rejected id costs one extra request, so a short window keeps that cost rare.
const TTL_SECS: u64 = 600;

/// The file of one memory server's session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionCache {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    session_id: String,
    saved_at_unix: u64,
}

fn now_unix() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

impl SessionCache {
    /// The cache of the server at `url`, in the state folder. The file name holds a hash of the
    /// URL, so a credential in the URL never reaches a file name. `None` when the state folder
    /// cannot be resolved.
    pub(crate) fn for_url(url: &str) -> Option<Self> {
        match llmenv_paths::state_dir() {
            Ok(dir) => Some(Self::at(dir.join(format!(
                "mcp-session-{}.json",
                crate::hook_run::record_hash::hash_prefix(url.as_bytes())
            )))),
            Err(e) => {
                tracing::warn!("the MCP session id is not shared between hooks: {e:#}");
                None
            }
        }
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The saved id, or `None` when no file exists, the file is unreadable, or the id is older
    /// than the window. A file that cannot be used is logged, and the caller negotiates a new
    /// session.
    pub(crate) fn load(&self) -> Option<String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", self.path.display());
                return None;
            }
        };
        let saved: Saved = match serde_json::from_slice(&bytes) {
            Ok(saved) => saved,
            Err(e) => {
                tracing::warn!("{} is not a saved MCP session: {e}", self.path.display());
                return None;
            }
        };
        let age = now_unix()?.saturating_sub(saved.saved_at_unix);
        // A saved time in the future gives age 0, so a bad clock cannot extend the window by
        // more than the skew itself. The server rejects a dead id either way.
        (age <= TTL_SECS && !saved.session_id.is_empty()).then_some(saved.session_id)
    }

    /// Saves `session_id`. A failed write is logged. The next process then negotiates its own
    /// session, which is the cost without the cache.
    pub(crate) fn save(&self, session_id: &str) {
        let Some(now) = now_unix() else {
            tracing::warn!("the system clock is before the Unix epoch, so no session id is saved");
            return;
        };
        let saved = Saved {
            session_id: session_id.to_string(),
            saved_at_unix: now,
        };
        let bytes = match serde_json::to_vec(&saved) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("cannot encode the MCP session id: {e}");
                return;
            }
        };
        // Unsynced: a power loss leaves a file that `load` reads as missing, and the cost is one
        // `initialize` round trip.
        if let Err(e) = llmenv_paths::write_owner_only_atomic_unsynced(&self.path, &bytes) {
            tracing::warn!(
                "cannot save the MCP session id to {}: {e}",
                self.path.display()
            );
        }
    }

    /// Deletes the saved id. The server rejected it, so no later process may reuse it.
    pub(crate) fn forget(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("cannot delete {}: {e}", self.path.display()),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    fn cache() -> (tempfile::TempDir, SessionCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = SessionCache::at(dir.path().join("s.json"));
        (dir, cache)
    }

    #[test]
    fn a_saved_id_loads_back_and_a_forgotten_one_does_not() {
        let (_dir, cache) = cache();
        assert_eq!(cache.load(), None);
        cache.save("abc");
        assert_eq!(cache.load().as_deref(), Some("abc"));
        cache.forget();
        assert_eq!(cache.load(), None);
        cache.forget();
    }

    #[test]
    fn an_id_older_than_the_window_is_not_trusted() {
        let (_dir, cache) = cache();
        let old = Saved {
            session_id: "abc".into(),
            saved_at_unix: now_unix().unwrap() - TTL_SECS - 5,
        };
        std::fs::write(&cache.path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(cache.load(), None);
    }

    #[test]
    fn a_corrupt_file_and_an_empty_id_load_as_missing() {
        let (_dir, cache) = cache();
        std::fs::write(&cache.path, b"not json").unwrap();
        assert_eq!(cache.load(), None);
        cache.save("");
        assert_eq!(cache.load(), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, cache) = cache();
        cache.save("abc");
        let mode = std::fs::metadata(&cache.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn two_urls_get_two_files_and_the_url_is_not_in_the_name() {
        let a = SessionCache::for_url("http://user:secret@h:1/mcp").unwrap();
        let b = SessionCache::for_url("http://user:secret@h:2/mcp").unwrap();
        assert_ne!(a, b);
        let name = a.path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !name.contains("secret") && name.starts_with("mcp-session-"),
            "{name}"
        );
    }
}
