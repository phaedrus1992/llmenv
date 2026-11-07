//! The per-session read cache of the read-once hook (#318).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{ReadEntry, read_once_state_dir, session_cache_path};
use crate::hook_run::session_state::unix_now;

/// Per-session cache of read-once tracked file reads. Stored as a flat JSON
/// file under `state_dir/read_once/{session_id}.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCache {
    /// Claude Code session id.
    pub session_id: String,
    /// Entries keyed by canonicalized file path.
    pub entries: HashMap<String, ReadEntry>,
    /// Set when the cache file exists but cannot be read. A save then would replace the
    /// unread file with an empty cache, so `save` skips the write (#2721).
    #[serde(skip)]
    pub(super) read_failed: bool,
}

impl SessionCache {
    /// Create a new empty session cache.
    pub(super) fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            entries: HashMap::new(),
            read_failed: false,
        }
    }

    /// Load the session cache from disk. Returns an empty cache on any IO or
    /// parse error (fail-soft). A read error also marks the cache so `save` leaves the file alone.
    pub fn load(state_dir: &Path, session_id: &str, ttl_seconds: u64) -> Self {
        let path = session_cache_path(state_dir, session_id);
        let mut cache: Self = match crate::paths::bounded::state_json_text_io(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::error!(
                    "failed to parse read-once cache {}, starting empty: {e}",
                    path.display()
                );
                crate::agent_notice::record(format!(
                    "llmenv: the read-once cache {} could not be parsed, so it starts empty. Fix: \
                     delete the file; the cache starts again on the next read.",
                    path.display()
                ));
                Self::new(session_id)
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(session_id),
            Err(e) => {
                tracing::error!(
                    "failed to read read-once cache {}, saves are skipped: {e}",
                    path.display()
                );
                crate::agent_notice::record(format!(
                    "llmenv: the read-once cache {} could not be read, so repeat reads are not \
                     tracked this session. Fix: run `llmenv doctor` to check the state directory.",
                    path.display()
                ));
                Self {
                    read_failed: true,
                    ..Self::new(session_id)
                }
            }
        };
        cache.prune(ttl_seconds);
        cache
    }

    /// Save the session cache atomically to disk. A cache that failed to load is not
    /// written (#2721). Opportunistically prunes stale session files before writing.
    pub fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        if self.read_failed {
            tracing::error!(
                "read-once cache for session {} not saved, the file could not be read",
                self.session_id
            );
            crate::agent_notice::record(
                "llmenv: the read-once cache for this session was not saved, because its file \
                 could not be read. Fix: run `llmenv doctor` to check the state directory.",
            );
            return Ok(());
        }
        Self::prune_stale_sessions(state_dir, 7);
        let path = session_cache_path(state_dir, &self.session_id);
        let json = serde_json::to_string(&self)?;
        crate::paths::write_owner_only_atomic(&path, json.as_bytes())?;
        Ok(())
    }

    /// Remove entries where `now - first_read_at > ttl_seconds`.
    pub fn prune(&mut self, ttl_seconds: u64) {
        let now = unix_now();
        self.entries
            .retain(|_, entry| within_ttl(now, entry.first_read_at, ttl_seconds));
    }

    /// Scan `state_dir/read_once/` and delete `.json` files older than
    /// `max_age_days`. Runs opportunistically during save().
    pub fn prune_stale_sessions(state_dir: &Path, max_age_days: u64) {
        crate::hook_run::session_state::prune_stale_json_files(
            &read_once_state_dir(state_dir),
            max_age_days,
        );
        crate::hook_run::session_state::prune_orphan_locks(
            &read_once_state_dir(state_dir),
            max_age_days,
        );
    }
}

/// Whether an entry first read at `first_read_at` is still inside the TTL at `now`. An entry
/// whose age equals the TTL has expired.
pub(super) fn within_ttl(now: i64, first_read_at: i64, ttl_seconds: u64) -> bool {
    now.saturating_sub(first_read_at) < i64::try_from(ttl_seconds).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::within_ttl;

    #[test]
    fn an_entry_exactly_one_ttl_old_has_expired() {
        assert!(!within_ttl(1000, 900, 100));
        assert!(within_ttl(999, 900, 100));
    }

    #[test]
    fn a_ttl_too_large_for_i64_keeps_every_entry() {
        assert!(within_ttl(0, 0, u64::MAX));
    }
}
