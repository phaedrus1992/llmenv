//! The MCP session id, shared by the hook processes of one memory server (#2909).
//!
//! Each hook is a new process, so an id kept in memory is lost at once, and each process paid an
//! `initialize` round trip before its first call. A file in the state folder hands the id to the
//! next process. The server stays the judge of the id: a rejected id is forgotten, and the call
//! retries once with a new one.
//!
//! The file name holds a hash of the URL and nothing else, so the URL is the whole identity of
//! the cache. A client that sends per-identity headers MUST NOT use it. No hook client sets
//! headers.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// How long a saved id is trusted. The server expires its session on its own clock, and a
/// rejected id costs one extra request, so a short window keeps that cost rare.
const TTL_SECS: u64 = 600;

/// The longest id that is trusted. A real id is a UUID or a short token, and the id goes into a
/// request header, so an oversized value is a corrupt file.
const MAX_ID_BYTES: usize = 256;

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

/// True for an id that is safe in a request header: visible ASCII, not empty, and short.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && id.bytes().all(|b| b.is_ascii_graphic())
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
                crate::agent_notice::record_logged(format!(
                    "llmenv: the MCP session id is not shared between hooks, because the state \
                     folder cannot be resolved: {e:#}. Fix: set HOME, or set LLMENV_STATE_DIR to \
                     a valid absolute path."
                ));
                None
            }
        }
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Tells the agent about a file problem that makes every hook pay the `initialize` round
    /// trip, with the fix.
    fn notice(&self, what: &str, error: &dyn std::fmt::Display) {
        crate::agent_notice::record_logged(format!(
            "llmenv: the shared MCP session file {} {what}: {error}. Hooks open their own \
             session until this is fixed. Fix: run `llmenv doctor` to check the state folder.",
            self.path.display()
        ));
    }

    /// The saved record, or `None` when no file exists or the file cannot be used. A file that
    /// cannot be read is noticed. A file that does not parse is only logged, because the next
    /// save replaces it.
    fn read(&self) -> Option<Saved> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                self.notice("cannot be read", &e);
                return None;
            }
        };
        serde_json::from_slice(&bytes)
            .inspect_err(|e| {
                tracing::warn!("{} is not a saved MCP session: {e}", self.path.display())
            })
            .ok()
    }

    /// The saved id, or `None` when none is saved, the id is older than the window, or the id is
    /// not safe in a header.
    pub(crate) fn load(&self) -> Option<String> {
        let saved = self.read()?;
        let Some(now) = now_unix() else {
            self.notice(
                "cannot be aged",
                &"the system clock is before the Unix epoch",
            );
            return None;
        };
        // A saved time in the future is not trusted, so a clock that stepped back cannot keep
        // an id alive (#3907).
        let trusted = saved.saved_at_unix <= now && now - saved.saved_at_unix <= TTL_SECS;
        (trusted && is_valid_id(&saved.session_id)).then_some(saved.session_id)
    }

    /// Saves `session_id`. A failed write is noticed, and the next process negotiates its own
    /// session, which is the cost without the cache.
    pub(crate) fn save(&self, session_id: &str) {
        if let Err(e) = self.try_save(session_id) {
            self.notice("was not saved", &format!("{e:#}"));
        }
    }

    fn try_save(&self, session_id: &str) -> anyhow::Result<()> {
        let saved_at_unix = now_unix().context("the system clock is before the Unix epoch")?;
        let saved = Saved {
            session_id: session_id.to_string(),
            saved_at_unix,
        };
        let bytes = serde_json::to_vec(&saved).context("cannot encode the session id")?;
        // Unsynced: a power loss leaves a file that `load` reads as missing, and the cost is one
        // `initialize` round trip.
        llmenv_paths::write_owner_only_atomic_unsynced(&self.path, &bytes)
            .context("cannot write the file")
    }

    /// Deletes the saved id when it is `rejected`. A newer id that another process saved stays.
    pub(crate) fn forget_if(&self, rejected: &str) {
        if self
            .read()
            .is_some_and(|saved| saved.session_id == rejected)
        {
            self.forget();
        }
    }

    /// Deletes the saved id, whatever it is.
    pub(crate) fn forget(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => self.notice("cannot be deleted", &e),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

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
    fn a_corrupt_file_and_an_unsafe_id_load_as_missing() {
        let (_dir, cache) = cache();
        std::fs::write(&cache.path, b"not json").unwrap();
        assert_eq!(cache.load(), None);
        for bad in ["", "a b", "a\nb", "a\u{e9}", &"x".repeat(MAX_ID_BYTES + 1)] {
            cache.save(bad);
            assert_eq!(cache.load(), None, "{bad:?}");
        }
        cache.save(&"x".repeat(MAX_ID_BYTES));
        assert!(cache.load().is_some());
    }

    #[test]
    fn forget_if_removes_only_the_rejected_id() {
        let (_dir, cache) = cache();
        cache.save("newer");
        cache.forget_if("older");
        assert_eq!(cache.load().as_deref(), Some("newer"));
        cache.forget_if("newer");
        assert_eq!(cache.load(), None);
        cache.forget_if("newer");
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
    fn a_file_that_cannot_be_saved_or_read_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file belongs makes the read fail, and so does the write.
        let path = dir.path().join("s.json");
        std::fs::create_dir(&path).unwrap();
        let cache = SessionCache::at(path);
        assert_eq!(cache.load(), None);
        cache.save("abc");
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("shared MCP session file"), "{notices}");
        assert!(notices.contains("llmenv doctor"), "{notices}");
    }

    proptest::proptest! {
        /// Any id that is safe in a header comes back unchanged.
        #[test]
        fn a_valid_id_round_trips(id in "[!-~]{1,256}") {
            let (_dir, cache) = cache();
            cache.save(&id);
            proptest::prop_assert_eq!(cache.load(), Some(id));
        }

        /// A record is trusted when its age is inside the window. A time in the future is not
        /// trusted. The offsets stay clear of the window edge, so the clock cannot flip a case.
        #[test]
        fn the_window_decides_whether_a_record_is_trusted(
            age in prop_oneof![0u64..590, 612u64..5_000],
            future in any::<bool>(),
            id in "[!-~]{1,40}",
        ) {
            let (_dir, cache) = cache();
            let now = now_unix().unwrap();
            let saved_at_unix = if future { now + age } else { now - age };
            let saved = Saved { session_id: id.clone(), saved_at_unix };
            std::fs::write(&cache.path, serde_json::to_vec(&saved).unwrap()).unwrap();
            let trusted = if future { age == 0 } else { age <= TTL_SECS };
            proptest::prop_assert_eq!(cache.load(), trusted.then_some(id));
        }

        /// Whatever the file holds, a load never panics.
        #[test]
        fn any_file_content_loads_without_a_panic(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
            let (_dir, cache) = cache();
            std::fs::write(&cache.path, bytes).unwrap();
            let _ = cache.load();
        }
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
