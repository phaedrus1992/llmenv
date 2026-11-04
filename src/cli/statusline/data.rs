//! `llmenv-status.json` — llmenv-sourced stats consumed by the statusline
//! renderer. Pure parsing only: no scope resolution, no MCP calls, no
//! business logic. All fields written once at data-file-write time by
//! `src/materialize/status_data.rs`.

use serde::{Deserialize, Serialize};

pub use super::session_tasks::SessionTasks;

// `Serialize` is derived alongside `Deserialize` on every type in this module
// (not just `StatusData`) so `crate::materialize::status_data` — the writer
// side — can construct and serialize these exact types instead of maintaining
// a second, parallel set of structs that could drift out of sync with what
// this module parses.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct StatusData {
    pub scopes: Option<ScopesData>,
    pub plugins: Option<CountData>,
    pub mcps: Option<CountData>,
    pub icm: Option<IcmData>,
    pub throttle: Option<ThrottleData>,
    /// Set when the stored throttle state exists but cannot be read. The
    /// widget shows this instead of reporting throttling as off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_error: Option<String>,
    /// Set when more than one throttle entry is active at once. This is a config conflict, not an
    /// unreadable state file, so the widget labels it apart from `throttle_error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_conflict: Option<String>,
    pub config_stale: Option<bool>,
    pub cache: Option<CacheData>,
    pub session_log: Option<u64>,
    pub tasks: Option<TasksData>,
    /// Set by [`StatusData::load`] when the file exists but cannot be read or parsed. It is
    /// never written to the file, so a file cannot set it.
    #[serde(skip)]
    pub unreadable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ScopesData {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CountData {
    pub total: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct IcmData {
    pub memories: u64,
    /// `None` when the stats text has no Topics line. That count is unknown, not zero.
    pub concepts: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ThrottleData {
    pub backend: String,
    pub cooldown_secs: u64,
}

/// Prunable cache size. `prunable_bytes` is `None` when the size could not be
/// read, so the status line shows an unknown marker rather than a zero.
/// `prunable_partial` is true when some entry was left out, so the total is a
/// floor and the status line shows it as "at least" (#3069). Older status data
/// has no such field, and it reads as an exact figure.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CacheData {
    pub prunable_bytes: Option<u64>,
    #[serde(default)]
    pub prunable_partial: bool,
}

/// Task-tracker progress, scoped to sessions open for the current project
/// (mandatory-sessions design). `session` is `None` when zero sessions are
/// open for this project (render empty — no active work tracked here);
/// `Some` gives the summed `(done, total)` across every session open for
/// this project (a single open session's own totals when there's just one).
/// `current` is the title of the most recently updated `wip`/`waiting` task
/// among those sessions — `None` when nothing is currently in progress.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TasksData {
    pub session: Option<SessionProgress>,
    pub current: Option<String>,
    /// Set when the task or session files cannot be read. The widget shows this instead of
    /// an empty or lower count, which would look like no work (#2687).
    #[serde(default)]
    pub unreadable: bool,
    /// Each open session of the project. Empty in a file from an older llmenv (#3196).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionTasks>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct SessionProgress {
    pub done: u64,
    pub total: u64,
}

impl StatusData {
    /// Load and parse `llmenv-status.json` at `path`. Never fails: every problem yields
    /// `StatusData::default()` (every field `None`), so the renderer's llmenv-sourced widgets
    /// render empty rather than aborting the whole statusline. A missing file is the normal
    /// empty case. Any other failure is also logged at error level, which the default log
    /// filter shows, so an unreadable status file does not pass as no data (#2712).
    #[must_use]
    pub fn load(path: &std::path::Path) -> Self {
        match crate::paths::bounded::state_bytes_io(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::error!("status file {} is not valid JSON: {e}", path.display());
                Self::unreadable()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::error!("cannot read status file {}: {e}", path.display());
                Self::unreadable()
            }
        }
    }

    /// The default data with [`StatusData::unreadable`] set, for a file that exists but fails.
    fn unreadable() -> Self {
        Self {
            unreadable: true,
            ..Self::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn load_parses_full_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(
            &path,
            r#"{
                "$schema": "llmenv-status-v1", "v": 1, "ts": "2026-07-15T14:23:00Z",
                "scopes": { "tags": ["dev", "rust"] },
                "plugins": { "total": 12, "errors": 0 },
                "mcps": { "total": 12, "errors": 0 },
                "icm": { "memories": 142, "concepts": 47 },
                "throttle": null,
                "config_stale": false,
                "cache": { "prunable_bytes": 15728640 },
                "session_log": 8
            }"#,
        )
        .unwrap();
        let data = StatusData::load(&path);
        assert_eq!(data.scopes.unwrap().tags, vec!["dev", "rust"]);
        assert_eq!(data.plugins.unwrap().total, 12);
        assert_eq!(data.icm.unwrap().memories, 142);
        assert_eq!(data.config_stale, Some(false));
        assert_eq!(data.cache.unwrap().prunable_bytes, Some(15_728_640));
        assert_eq!(data.session_log, Some(8));
        assert!(data.throttle.is_none());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let data = StatusData::load(std::path::Path::new("/nonexistent/llmenv-status.json"));
        assert_eq!(data, StatusData::default());
    }

    #[test]
    fn a_status_file_cannot_set_the_unreadable_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, br#"{"unreadable": true}"#).unwrap();
        assert!(!StatusData::load(&path).unreadable);
    }

    #[test]
    fn load_corrupt_file_returns_default_marked_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let data = StatusData::load(&path);
        assert_eq!(
            data,
            StatusData {
                unreadable: true,
                ..StatusData::default()
            }
        );
    }

    #[test]
    fn load_partial_json_defaults_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, r#"{"session_log": 3}"#).unwrap();
        let data = StatusData::load(&path);
        assert_eq!(data.session_log, Some(3));
        assert!(data.scopes.is_none());
        assert!(data.plugins.is_none());
    }

    #[test]
    fn load_reads_a_file_that_has_no_throttle_conflict_key() {
        // A status file written before `throttle_conflict` existed must still load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, r#"{"throttle_error": "parsing throttle state: /x"}"#).unwrap();
        let data = StatusData::load(&path);
        assert_eq!(
            data.throttle_error.as_deref(),
            Some("parsing throttle state: /x")
        );
        assert_eq!(data.throttle_conflict, None);
    }

    use proptest::prelude::*;

    fn arb_status_data() -> impl Strategy<Value = StatusData> {
        (
            proptest::option::of(proptest::collection::vec("[a-z]{1,8}", 0..4)),
            proptest::option::of((any::<u64>(), any::<u64>())),
            proptest::option::of(any::<bool>()),
            proptest::option::of(any::<u64>()),
            proptest::option::of(("[a-z]{1,8}", any::<u64>())),
            proptest::option::of("[ -~]{0,20}"),
            proptest::option::of("[ -~]{0,20}"),
        )
            .prop_map(
                |(
                    tags,
                    plugins,
                    config_stale,
                    session_log,
                    throttle,
                    throttle_error,
                    throttle_conflict,
                )| StatusData {
                    scopes: tags.map(|tags| ScopesData { tags }),
                    plugins: plugins.map(|(total, errors)| CountData { total, errors }),
                    mcps: None,
                    icm: None,
                    throttle: throttle.map(|(backend, cooldown_secs)| ThrottleData {
                        backend,
                        cooldown_secs,
                    }),
                    throttle_error,
                    throttle_conflict,
                    config_stale,
                    cache: None,
                    session_log,
                    tasks: None,
                    unreadable: false,
                },
            )
    }

    proptest! {
        #[test]
        fn load_roundtrips_arbitrary_serialized_status_data(data in arb_status_data()) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("llmenv-status.json");
            let json = serde_json::to_vec(&data).unwrap();
            std::fs::write(&path, json).unwrap();
            prop_assert_eq!(StatusData::load(&path), data);
        }

        #[test]
        fn load_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("llmenv-status.json");
            std::fs::write(&path, &bytes).unwrap();
            // Must degrade to default, never panic, on arbitrary (likely
            // non-JSON) bytes.
            let _ = StatusData::load(&path);
        }
    }
}
