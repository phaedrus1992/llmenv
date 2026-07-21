//! `llmenv-status.json` — llmenv-sourced stats consumed by the statusline
//! renderer. Pure parsing only: no scope resolution, no MCP calls, no
//! business logic. All fields written once at data-file-write time by
//! `src/materialize/status_data.rs`.

use serde::{Deserialize, Serialize};

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
    pub config_stale: Option<bool>,
    pub cache: Option<CacheData>,
    pub session_log: Option<u64>,
    pub tasks: Option<TasksData>,
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
    pub concepts: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ThrottleData {
    pub backend: String,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CacheData {
    pub prunable_bytes: u64,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct SessionProgress {
    pub done: u64,
    pub total: u64,
}

impl StatusData {
    /// Load and parse `llmenv-status.json` at `path`. Never fails: a missing
    /// file, unreadable file, or parse error all yield `StatusData::default()`
    /// (every field `None`) so the renderer's llmenv-sourced widgets simply
    /// render empty rather than aborting the whole statusline.
    #[must_use]
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
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
        assert_eq!(data.cache.unwrap().prunable_bytes, 15_728_640);
        assert_eq!(data.session_log, Some(8));
        assert!(data.throttle.is_none());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let data = StatusData::load(std::path::Path::new("/nonexistent/llmenv-status.json"));
        assert_eq!(data, StatusData::default());
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let data = StatusData::load(&path);
        assert_eq!(data, StatusData::default());
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

    use proptest::prelude::*;

    fn arb_status_data() -> impl Strategy<Value = StatusData> {
        (
            proptest::option::of(proptest::collection::vec("[a-z]{1,8}", 0..4)),
            proptest::option::of((any::<u64>(), any::<u64>())),
            proptest::option::of(any::<bool>()),
            proptest::option::of(any::<u64>()),
        )
            .prop_map(|(tags, plugins, config_stale, session_log)| StatusData {
                scopes: tags.map(|tags| ScopesData { tags }),
                plugins: plugins.map(|(total, errors)| CountData { total, errors }),
                mcps: None,
                icm: None,
                throttle: None,
                config_stale,
                cache: None,
                session_log,
                tasks: None,
            })
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
