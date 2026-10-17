//! Persists the `claude_session_id -> icm_session_id` correlation under the
//! stable state dir so every hook process for a launch records into the same
//! transcript session.
//!
//! The public functions resolve `state_dir()`; the `*_at` helpers take an
//! explicit path so tests exercise the on-disk format without touching the
//! global state-dir env var (mirrors `crate::icm`'s `write_memory`/`read_memory`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use llmenv_paths::{state_dir, write_owner_only_atomic};

mod link;
pub(crate) use link::{Link, claim_pending_at, clear_pending_at, link_at};

/// Cap on the correlation map's entry count (#509 item 2). Nothing ever
/// removes an entry, so a long-lived install accumulates them forever
/// without a bound. This caps *concurrently open* sessions, not lifetime
/// session count — a session whose `SessionEnd` hasn't fired yet is still
/// evictable (see the eviction-order comment in `record_at`). 1000
/// simultaneously open sessions is unrealistic for a single install.
const MAX_CORRELATION_ENTRIES: usize = 1000;

/// Path to the correlation map file.
///
/// # Errors
/// The state dir cannot be resolved, for example when `$HOME` is not set. A
/// CWD-relative map would land in the user's project, so there is no fallback (#2668).
pub(crate) fn state_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("transcript-sessions.json"))
}

/// Load the correlation map at `path`. A missing file is the normal
/// first-run case and returns an empty map silently. A file that exists but
/// fails to parse (truncated by a crash, hand-edited, corrupted) is an error.
/// A lookup must not treat it as a miss (#2658). A file that exists but
/// cannot be read is an error too. Writes use [`load_for_write`], which moves a corrupt
/// file aside instead of failing.
fn load_at(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let Some(s) = read_map_text(path)? else {
        return Ok(BTreeMap::default());
    };
    // A corrupt map is an error here, not a miss. A miss would end a session's
    // lifecycle link silently (#2658). The next write moves the file aside.
    serde_json::from_str(&s).with_context(|| {
        format!(
            "the session map {} is corrupt. The next write moves it aside to {}.corrupt",
            path.display(),
            path.display()
        )
    })
}

/// Read the map text at `path`. `None` means the file is missing.
fn read_map_text(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        // A missing file is the normal first run, so it is not a warning.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => anyhow::bail!(
            "cannot read the session map {}: {e}. Fix its permissions, or remove the file to \
             start over.",
            path.display()
        ),
    }
}

/// Load the map before a write. A file that does not parse is moved to
/// `transcript-sessions.json.corrupt` first. Without the move, the write would
/// replace the file and erase every other mapping in it (#2658).
fn load_for_write(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let Some(s) = read_map_text(path)? else {
        return Ok(BTreeMap::default());
    };
    match serde_json::from_str(&s) {
        Ok(map) => Ok(map),
        Err(e) => {
            // A second corruption must not overwrite the first aside copy (#2658 review).
            let mut aside = path.with_extension("json.corrupt");
            if aside.exists() {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                aside = path.with_extension(format!("json.corrupt.{nanos}"));
            }
            std::fs::rename(path, &aside).with_context(|| {
                format!(
                    "moving the corrupt session map {} aside to {}",
                    path.display(),
                    aside.display()
                )
            })?;
            // Error level: the default log filter is ERROR, so a warn is never shown (#2657).
            tracing::error!(
                error = %e,
                path = %path.display(),
                aside = %aside.display(),
                "corrupt transcript-sessions.json moved aside, starting a new map"
            );
            // The new map has no links, so every open session starts a new transcript (#2834).
            crate::agent_notice::record(format!(
                "llmenv: the session map {} was corrupt ({e}), so it was moved aside to {} and a \
                 new map was started. Open sessions lose their ICM transcript link. Fix: look in \
                 the aside copy for the lost links, then remove it.",
                path.display(),
                aside.display()
            ));
            Ok(BTreeMap::default())
        }
    }
}

/// Lock key for the map file, so its lock is `transcript-sessions.lock` in the state dir.
const MAP_LOCK_KEY: &str = "transcript-sessions";

/// Hold the lock for the map at `path` across one read-modify-write. Without it, two hooks
/// can read the same map. A corrupt-map rename by one hook can then move the other hook's
/// fresh map aside (#2667).
fn lock_map(path: &Path) -> anyhow::Result<std::fs::File> {
    // No fallback to ".": locking creates the directory and resets its mode, and a bare file name
    // would put that in the user's project.
    let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        anyhow::bail!(
            "the session map path {} has no parent directory, so it cannot be locked",
            path.display()
        );
    };
    crate::hook_run::session_state::lock_state_file(dir, MAP_LOCK_KEY, "session map").ok_or_else(
        || {
            anyhow::anyhow!(
                "cannot lock the session map {}, so it was not written. Check the state dir \
                 permissions, and whether another llmenv hook is stuck.",
                path.display()
            )
        },
    )
}

/// Insert `key -> value` and keep the map within [`MAX_CORRELATION_ENTRIES`]. Expired pending
/// markers go first, because no record replaces a marker whose link failed (#2834).
fn insert_capped(map: &mut BTreeMap<String, String>, key: &str, value: &str, now: i64) {
    map.retain(|k, v| k == key || !link::is_expired_marker(v, now));
    // Evict before the insert. Evicting after it could drop the new key itself, when that key
    // sorts first, and the lookup for this session would then miss.
    if !map.contains_key(key) && map.len() >= MAX_CORRELATION_ENTRIES {
        map.pop_first();
    }
    map.insert(key.to_string(), value.to_string());
    // ponytail: eviction order is BTreeMap key order (the random session-id
    // string), not recency — this format carries no timestamp to sort by.
    // Correlation entries are looked up by exact claude_session_id, and an
    // evicted-but-still-live session just starts a fresh transcript on its
    // next lookup miss (no data loss, no error). Upgrade to a timestamped
    // structure if true LRU ever matters.
    while map.len() > MAX_CORRELATION_ENTRIES {
        map.pop_first();
    }
}

fn record_at(path: &Path, claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()> {
    let _lock = lock_map(path)?;
    let mut map = load_for_write(path)?;
    let now = crate::hook_run::session_state::unix_now();
    insert_capped(&mut map, claude_session_id, icm_session_id, now);
    let body = serde_json::to_string(&map)?;
    write_owner_only_atomic(path, body.as_bytes())?;
    Ok(())
}

/// The ICM session id for a Claude session, if recorded in the map at `path`. A pending
/// marker is not an id, so it reads as `None`. Tests only; the hook reads [`link_at`].
///
/// # Errors
/// The map file exists but cannot be read.
#[cfg(test)]
pub(crate) fn lookup_session_at(
    path: &Path,
    claude_session_id: &str,
) -> anyhow::Result<Option<String>> {
    let now = crate::hook_run::session_state::unix_now();
    Ok(match link_at(path, claude_session_id, now)? {
        Link::Linked(id) => Some(id),
        Link::Missing | Link::Pending { .. } => None,
    })
}

/// Record the correlation into the map at `path` (read-modify-write, atomic,
/// 0o600). See [`lookup_session_at`] for why this takes an explicit path.
///
/// # Errors
/// Directory creation or atomic-write failure.
pub(crate) fn record_session_at(
    path: &Path,
    claude_session_id: &str,
    icm_session_id: &str,
) -> anyhow::Result<()> {
    record_at(path, claude_session_id, icm_session_id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn record_then_lookup_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        assert!(load_at(&path).unwrap().is_empty());
        record_at(&path, "claude-1", "icm-aaa").unwrap();
        record_at(&path, "claude-2", "icm-bbb").unwrap();
        let map = load_at(&path).unwrap();
        assert_eq!(map.get("claude-1").map(String::as_str), Some("icm-aaa"));
        assert_eq!(map.get("claude-2").map(String::as_str), Some("icm-bbb"));
        assert_eq!(map.get("missing"), None);
    }

    #[test]
    fn record_at_caps_correlation_map_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        for i in 0..MAX_CORRELATION_ENTRIES + 10 {
            record_at(&path, &format!("claude-{i:05}"), "icm-x").unwrap();
        }
        let map = load_at(&path).unwrap();
        assert_eq!(map.len(), MAX_CORRELATION_ENTRIES);
    }

    #[test]
    fn load_at_errors_on_corrupt_json_instead_of_reading_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let result = lookup_session_at(&path, "claude-1");
        assert!(result.is_err(), "corrupt must not be a miss: {result:?}");
        let text = format!("{:#}", result.unwrap_err());
        assert!(text.contains(&path.display().to_string()), "{text}");
    }

    proptest! {
        /// Any content that is not a JSON map is kept byte for byte in the aside file,
        /// and the write that moved it still records its pair (#2658).
        #[test]
        fn a_corrupt_map_is_kept_byte_for_byte_in_the_aside_file(junk in "#[ -~]{1,64}") {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("transcript-sessions.json");
            std::fs::write(&path, &junk).unwrap();
            record_at(&path, "claude-new", "icm-new").unwrap();
            let aside = dir.path().join("transcript-sessions.json.corrupt");
            prop_assert_eq!(std::fs::read_to_string(&aside).unwrap(), junk);
            let found = lookup_session_at(&path, "claude-new").unwrap();
            prop_assert_eq!(found.as_deref(), Some("icm-new"));
        }
    }

    /// Write-only mode makes the read fail while a write still succeeds. Before the fix the record
    /// wrote back an empty map and erased both stored mappings (#2612).
    #[cfg(unix)]
    #[test]
    fn a_read_error_does_not_erase_the_stored_map() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        record_at(&path, "a", "icm-a").unwrap();
        record_at(&path, "b", "icm-b").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        let result = record_at(&path, "c", "icm-c");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err(), "an unreadable map must fail the record");
        assert_eq!(
            lookup_session_at(&path, "a").unwrap().as_deref(),
            Some("icm-a")
        );
        assert_eq!(
            lookup_session_at(&path, "b").unwrap().as_deref(),
            Some("icm-b")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_lookup_on_an_unreadable_map_is_an_error_not_a_miss() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        record_at(&path, "a", "icm-a").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        let result = lookup_session_at(&path, "a");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // A miss would start a new transcript that can never be linked (#2658).
        assert!(
            result.is_err(),
            "an unreadable map must not look like a miss"
        );
    }

    #[test]
    fn a_corrupt_map_is_moved_aside_before_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        std::fs::write(&path, "{not valid json").unwrap();
        record_at(&path, "claude-new", "icm-new").unwrap();

        let aside = dir.path().join("transcript-sessions.json.corrupt");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{not valid json");
        assert_eq!(
            lookup_session_at(&path, "claude-new").unwrap().as_deref(),
            Some("icm-new")
        );
        // The agent must hear that open sessions lost their links (#2834).
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(notices.contains(&aside.display().to_string()), "{notices}");
    }

    #[test]
    fn a_new_key_that_sorts_first_survives_a_full_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        for i in 0..MAX_CORRELATION_ENTRIES {
            record_at(&path, &format!("claude-{i:05}"), "icm-old").unwrap();
        }
        // "aaa" sorts before every "claude-" key, so pop_first would have dropped it at once.
        record_at(&path, "aaa", "icm-new").unwrap();
        assert_eq!(
            lookup_session_at(&path, "aaa").unwrap().as_deref(),
            Some("icm-new")
        );
        assert_eq!(load_at(&path).unwrap().len(), MAX_CORRELATION_ENTRIES);
    }

    #[test]
    fn record_fails_while_another_writer_holds_the_map_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        let _held = crate::hook_run::session_state::lock_state_file(
            dir.path(),
            MAP_LOCK_KEY,
            "test holder",
        )
        .unwrap();
        let err = record_at(&path, "claude-1", "icm-a").unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot lock the session map"),
            "{err:#}"
        );
        assert!(!path.exists(), "a write under a busy lock must not happen");
    }

    #[test]
    fn a_bare_file_name_is_refused_before_anything_is_created() {
        // A bare name has no parent. The lock must not fall back to the working directory.
        let err =
            record_at(Path::new("transcript-sessions.json"), "claude-1", "icm-a").unwrap_err();
        assert!(
            format!("{err:#}").contains("has no parent directory"),
            "{err:#}"
        );
        assert!(!Path::new("transcript-sessions.json").exists());
        assert!(!Path::new("transcript-sessions.lock").exists());
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn record_then_load_returns_every_pair_recorded(
            pairs in prop::collection::vec(("\\PC{1,16}", "\\PC{0,16}"), 0..8),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("transcript-sessions.json");
            let mut expected = BTreeMap::new();
            for (claude, icm) in &pairs {
                record_at(&path, claude, icm).unwrap();
                expected.insert(claude.clone(), icm.clone());
            }
            prop_assert_eq!(load_at(&path).unwrap(), expected);
        }

        #[test]
        fn state_file_is_owner_only(id in "[a-z0-9-]{1,16}") {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("transcript-sessions.json");
            record_at(&path, &id, "icm-x").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                prop_assert_eq!(mode & 0o077, 0);
            }
        }
    }
}
