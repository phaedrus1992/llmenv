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
    match crate::paths::bounded::state_text_io(path) {
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

/// Create an empty file at `aside` only if nothing is there. `create_new` makes the check and
/// the create one step, so another llmenv cannot take the name in between (#3121).
fn create_new_empty(aside: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(aside)
        .map(|_| ())
}

/// Reserve a name for a corrupt map: `.json.corrupt`, or a nanosecond-suffixed name when that
/// one is taken. The empty reservation file is replaced by the rename that follows, so the
/// name stays ours from reservation to rename.
fn reserve_aside_name(path: &Path) -> anyhow::Result<PathBuf> {
    let base = path.with_extension("json.corrupt");
    match create_new_empty(&base) {
        Ok(()) => return Ok(base),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "the session map {} cannot be moved aside: its aside name {} cannot be \
                     reserved. Fix: check the permissions of the folder that holds the map, then \
                     retry.",
                    path.display(),
                    base.display()
                )
            });
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let fallback = path.with_extension(format!("json.corrupt.{nanos}"));
    create_new_empty(&fallback).with_context(|| {
        format!(
            "the session map {} cannot be moved aside: its aside name {} cannot be reserved. \
             Fix: check the permissions of the folder that holds the map, then retry.",
            path.display(),
            fallback.display()
        )
    })?;
    Ok(fallback)
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
            let aside = reserve_aside_name(path).with_context(|| {
                format!(
                    "the session map {} is corrupt ({e}), so it was not moved",
                    path.display()
                )
            })?;
            if let Err(e) = std::fs::rename(path, &aside) {
                // The reservation is ours and empty. Drop it so a failed move leaves no stray file.
                // The rename error is the one the caller needs. A failed cleanup is a notice,
                // because the default log filter hides warn lines (#2657).
                if let Err(cleanup) = std::fs::remove_file(&aside) {
                    crate::agent_notice::record_logged(format!(
                        "llmenv: could not remove the empty aside reservation {} ({cleanup}). \
                         Fix: remove that file by hand.",
                        aside.display()
                    ));
                }
                return Err(e).with_context(|| {
                    format!(
                        "moving the corrupt session map {} aside to {}",
                        path.display(),
                        aside.display()
                    )
                });
            }
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
mod tests;
