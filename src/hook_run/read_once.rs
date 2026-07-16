//! Read-once file deduplication hook (#318).
//!
//! Tracks which files have been read via `PreToolUse`/`Read` tool calls,
//! cached per-session in a flat JSON file under `state_dir/read_once/`.
//! Warns or denies redundant re-reads to save context-window tokens.
//!
//! Fail-soft: any cache/IO error logs to stderr and passes the read through
//! silently — the optimizer must never block real work.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ReadOnce as ReadOnceConfig;
use crate::config::ReadOnceMode;

/// A single tracked file read in the session cache.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadEntry {
    /// The file path as reported by the tool call (canonicalized for the key).
    pub path: String,
    /// File mtime as unix seconds when first read.
    pub mtime_unix: i64,
    /// Unix timestamp of the first read in this session.
    pub first_read_at: i64,
    /// How many times this path was re-read (cache hits).
    pub hits: u64,
    /// Estimated tokens saved from denying/warning re-reads.
    pub tokens_saved: u64,
}

/// Per-session cache of read-once tracked file reads. Stored as a flat JSON
/// file under `state_dir/read_once/{session_id}.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCache {
    /// Claude Code session id.
    pub session_id: String,
    /// Entries keyed by canonicalized file path.
    pub entries: HashMap<String, ReadEntry>,
}

impl SessionCache {
    /// Create a new empty session cache.
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            entries: HashMap::new(),
        }
    }

    /// Load the session cache from disk. Returns an empty cache on any IO or
    /// parse error (fail-soft).
    pub fn load(state_dir: &Path, session_id: &str, ttl_seconds: u64) -> Self {
        let path = session_cache_path(state_dir, session_id);
        let mut cache: Self = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!("llmenv: failed to parse read-once cache: {e}");
                Self::new(session_id)
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(session_id),
            Err(e) => {
                eprintln!(
                    "llmenv: failed to read read-once cache {}: {e}",
                    path.display()
                );
                Self::new(session_id)
            }
        };
        cache.prune(ttl_seconds);
        cache
    }

    /// Save the session cache atomically to disk. Logs errors to stderr
    /// (fail-soft). Opportunistically prunes stale session files before writing.
    pub fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        Self::prune_stale_sessions(state_dir, 7);
        let ro_dir = read_once_state_dir(state_dir);
        std::fs::create_dir_all(&ro_dir)?;

        let path = session_cache_path(state_dir, &self.session_id);
        let json = serde_json::to_string(&self)?;
        crate::paths::write_owner_only_atomic(&path, json.as_bytes())?;
        Ok(())
    }

    /// Remove entries where `now - first_read_at > ttl_seconds`.
    pub fn prune(&mut self, ttl_seconds: u64) {
        let now = unix_now();
        self.entries.retain(|_, entry| {
            let age = now.saturating_sub(entry.first_read_at);
            age < ttl_seconds as i64
        });
    }

    /// Scan `state_dir/read_once/` and delete `.json` files older than
    /// `max_age_days`. Runs opportunistically during save().
    pub fn prune_stale_sessions(state_dir: &Path, max_age_days: u64) {
        let max_age_secs = max_age_days * 86_400;
        let now = unix_now();
        let ro_dir = read_once_state_dir(state_dir);
        let entries = match std::fs::read_dir(&ro_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!("llmenv: failed to read read-once dir for pruning: {e}");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&path).inspect_err(|e| {
                tracing::warn!(
                    "prune_stale_sessions: stat failed for {}: {e}",
                    path.display()
                )
            }) && let Ok(modified) = meta.modified().inspect_err(|e| {
                tracing::warn!(
                    "prune_stale_sessions: mtime failed for {}: {e}",
                    path.display()
                )
            }) && let Ok(duration) =
                modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .inspect_err(|e| {
                        tracing::warn!(
                            "prune_stale_sessions: duration_since failed for {}: {e}",
                            path.display()
                        )
                    })
            {
                let age_secs = now.saturating_sub(duration.as_secs() as i64);
                if age_secs > max_age_secs as i64
                    && let Err(e) = std::fs::remove_file(&path)
                {
                    eprintln!("llmenv: failed to prune stale read-once cache: {e}");
                }
            }
        }
    }
}

/// Build the state subdirectory path for read-once cache files.
pub fn read_once_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("read_once")
}

/// Build the full path to a session's cache file.
fn session_cache_path(state_dir: &Path, session_id: &str) -> PathBuf {
    read_once_state_dir(state_dir).join(format!("{session_id}.json"))
}

/// Clear the entire read-once cache by removing the state directory.
pub fn clear_cache() -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    let ro_dir = read_once_state_dir(&state_dir);
    if ro_dir.exists() {
        std::fs::remove_dir_all(&ro_dir)?;
        writeln!(std::io::stdout(), "Read-once cache cleared")?;
    } else {
        writeln!(std::io::stdout(), "No read-once cache to clear")?;
    }
    Ok(())
}

/// Return the current unix timestamp as i64.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Handle a PreToolUse event for the read-once feature.
///
/// Returns:
/// - Empty string → pass the read through (first read, changed file, partial
///   read, missing file, or any error).
/// - Advisory text → warn mode, the read is allowed but we emit an advisory.
/// - `"__DENY__:<reason>"` → deny mode, caller should emit a deny envelope.
///
/// This function is extracted so unit tests can drive it directly without
/// needing a full hook-run invocation.
pub fn handle_pre_tool_use(
    stdin_payload: &serde_json::Value,
    session_id: Option<&str>,
    config: &ReadOnceConfig,
) -> String {
    let Ok(state_dir) = crate::paths::state_dir().inspect_err(|e| {
        tracing::warn!("failed to resolve state_dir for read-once pre-tool-use: {e}")
    }) else {
        return String::new();
    };
    handle_pre_tool_use_inner(stdin_payload, session_id, config, &state_dir)
}

/// Like [`handle_pre_tool_use`] but with an injectable `state_dir` for testing.
pub(crate) fn handle_pre_tool_use_inner(
    stdin_payload: &serde_json::Value,
    session_id: Option<&str>,
    config: &ReadOnceConfig,
    state_dir: &Path,
) -> String {
    // Only handle Read tool calls
    if stdin_payload["tool_name"].as_str() != Some("Read") {
        return String::new();
    }
    // Parse tool_input for file path and offset/limit
    let tool_input = match stdin_payload["tool_input"].as_object() {
        Some(obj) => obj,
        None => return String::new(),
    };
    // Extract file path — the real key is snake_case per Claude Code hook payload
    // convention, but accept PascalCase fallback for synthetic test payloads.
    let Some(file_path) = tool_input
        .get("file_path")
        .or_else(|| {
            let v = tool_input.get("filePath");
            if v.is_some() {
                eprintln!("llmenv: read-once using deprecated PascalCase 'filePath' key instead of 'file_path'");
            }
            v
        })
        .and_then(|v| v.as_str())
    else {
        return String::new();
    };
    // Partial read bypass: if offset or limit are present, never cache
    if tool_input.contains_key("offset") || tool_input.contains_key("limit") {
        return String::new();
    }
    // Stat the file
    let path = Path::new(file_path);
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("read_once stat failed for {}: {e}", path.display());
            return String::new();
        }
    };
    let mtime = metadata
        .modified()
        .inspect_err(|e| tracing::warn!("read_once mtime failed for {}: {e}", path.display()))
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let file_size = metadata.len(); // Need a session id to cache
    let session_id = match session_id {
        Some(id) => id,
        None => return String::new(),
    };
    let canonical = std::fs::canonicalize(path)
        .inspect_err(|e| {
            eprintln!(
                "llmenv: cannot canonicalize read-once cache path {}: {e}, using raw path",
                path.display()
            );
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "cannot canonicalize read-once cache path, using raw path"
            )
        })
        .unwrap_or_else(|_| path.to_path_buf());
    let path_key = canonical.to_string_lossy().to_string();
    // Load session cache
    let mut cache = SessionCache::load(state_dir, session_id, config.ttl_seconds);
    let now = unix_now();
    let tokens_saved = (file_size / 4) as u64;

    // Check existing entry
    if let Some(entry) = cache.entries.get_mut(&path_key) {
        if entry.mtime_unix == mtime
            && now.saturating_sub(entry.first_read_at) < config.ttl_seconds as i64
        {
            // Cache hit within TTL — warn or deny.
            // Don't save on hit — hits/tokens_saved are advisory stats that
            // get accurately written on the next miss-path save.
            entry.hits = entry.hits.saturating_add(1);
            entry.tokens_saved = entry.tokens_saved.saturating_add(tokens_saved);

            let msg = format!(
                "{file_path} was already read this session (~{tokens_saved} tokens saved from re-read). \
                 Prefer the copy already in context."
            );
            return if config.mode == ReadOnceMode::Deny {
                format!("__DENY__:{msg}")
            } else {
                msg
            };
        }
        // Cache miss (mtime changed or TTL expired) — update entry
        entry.mtime_unix = mtime;
        entry.first_read_at = now;
        entry.hits = 0;
        entry.tokens_saved = 0;
    } else {
        // New entry
        cache.entries.insert(
            path_key,
            ReadEntry {
                path: file_path.to_string(),
                mtime_unix: mtime,
                first_read_at: now,
                hits: 0,
                tokens_saved: 0,
            },
        );
    }

    if let Err(e) = cache.save(state_dir) {
        eprintln!("llmenv: failed to save read-once cache: {e}");
    }

    String::new() // Pass through
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn test_config_warn() -> ReadOnceConfig {
        ReadOnceConfig {
            enabled: true,
            mode: ReadOnceMode::Warn,
            ttl_seconds: 1200,
        }
    }

    fn test_config_deny() -> ReadOnceConfig {
        ReadOnceConfig {
            enabled: true,
            mode: ReadOnceMode::Deny,
            ttl_seconds: 1200,
        }
    }

    /// Build a synthetic PreToolUse stdin payload for a Read tool call.
    /// Uses snake_case keys matching what Claude Code actually sends.
    fn read_payload(path: &str) -> serde_json::Value {
        serde_json::json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": path,
            },
        })
    }

    /// Build a synthetic PreToolUse stdin payload with offset/limit.
    fn partial_read_payload(path: &str, offset: u64, limit: u64) -> serde_json::Value {
        serde_json::json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": path,
                "offset": offset,
                "limit": limit,
            },
        })
    }

    fn non_read_payload() -> serde_json::Value {
        crate::test_fixtures::load_hook_payload("edit.json")
    }

    #[test]
    fn edit_fixture_uses_snake_case_keys() {
        let payload = crate::test_fixtures::load_hook_payload("edit.json");
        let tool_input = &payload["tool_input"];
        assert!(
            tool_input.get("old_string").is_some(),
            "edit fixture must use snake_case old_string, not oldString"
        );
        assert!(
            tool_input.get("new_string").is_some(),
            "edit fixture must use snake_case new_string, not newString"
        );
    }

    #[test]
    fn non_read_tool_passes_through() {
        let result = handle_pre_tool_use(
            &non_read_payload(),
            Some("test-session"),
            &test_config_warn(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn partial_read_passes_through() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"hello world").expect("test");

        let result = handle_pre_tool_use(
            &partial_read_payload(file_path.to_str().expect("test"), 0, 10),
            Some("test-session"),
            &test_config_warn(),
        );
        assert!(result.is_empty(), "partial read should pass through");
    }

    #[test]
    fn missing_file_passes_through() {
        let result = handle_pre_tool_use(
            &read_payload("/nonexistent/file.txt"),
            Some("test-session"),
            &test_config_warn(),
        );
        assert!(result.is_empty(), "missing file should pass through");
    }

    #[test]
    fn no_session_id_passes_through() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"content").expect("test");

        let result = handle_pre_tool_use(
            &read_payload(file_path.to_str().expect("test")),
            None,
            &test_config_warn(),
        );
        assert!(result.is_empty(), "no session id should pass through");
    }

    #[test]
    fn first_read_passes_through() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"hello").expect("test");

        let result = handle_pre_tool_use_inner(
            &read_payload(file_path.to_str().expect("test")),
            Some("test-session"),
            &test_config_warn(),
            dir.path(),
        );
        assert!(result.is_empty(), "first read should pass through");
    }

    #[test]
    fn second_read_in_warn_mode_returns_advisory() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"hello world").expect("test");

        let payload = read_payload(file_path.to_str().expect("test"));

        // First read passes through
        let result1 =
            handle_pre_tool_use_inner(&payload, Some("test-warn"), &test_config_warn(), dir.path());
        assert!(result1.is_empty(), "first read should pass through");

        // Second read warns
        let result2 =
            handle_pre_tool_use_inner(&payload, Some("test-warn"), &test_config_warn(), dir.path());
        assert!(!result2.is_empty(), "second read should warn");
        assert!(
            !result2.contains("__DENY__"),
            "warn mode should not emit deny"
        );
        assert!(
            result2.contains("already read"),
            "advisory should mention re-read"
        );
    }

    #[test]
    fn second_read_in_deny_mode_returns_deny_marker() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test_deny.txt");
        fs::write(&file_path, b"hello world").expect("test");

        let payload = read_payload(file_path.to_str().expect("test"));

        // First read passes through
        let result1 =
            handle_pre_tool_use_inner(&payload, Some("test-deny"), &test_config_deny(), dir.path());
        assert!(result1.is_empty(), "first read should pass through");

        // Second read denies
        let result2 =
            handle_pre_tool_use_inner(&payload, Some("test-deny"), &test_config_deny(), dir.path());
        assert!(!result2.is_empty(), "second read should deny");
        assert!(
            result2.starts_with("__DENY__:"),
            "deny mode should return deny marker"
        );
    }

    #[test]
    fn changed_mtime_passes_through_again() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test_mtime.txt");
        fs::write(&file_path, b"v1").expect("test");

        let payload = read_payload(file_path.to_str().expect("test"));

        // First read
        let result1 = handle_pre_tool_use_inner(
            &payload,
            Some("test-mtime"),
            &test_config_warn(),
            dir.path(),
        );
        assert!(result1.is_empty());

        // Sleep to ensure mtime changes (sub-second writes don't always advance mtime
        // on coarse-granularity filesystems like tmpfs or ext4 with `ms` resolution).
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Modify file
        fs::write(&file_path, b"v2").expect("test");

        // Read again — mtime changed, should pass through even in deny mode
        let result2 = handle_pre_tool_use_inner(
            &payload,
            Some("test-mtime"),
            &test_config_deny(),
            dir.path(),
        );
        assert!(result2.is_empty(), "changed file should pass through");
    }

    #[test]
    fn ttl_expiry_passes_through_again() {
        let dir = TempDir::new().expect("test");
        let file_path = dir.path().join("test_ttl.txt");
        fs::write(&file_path, b"content").expect("test");

        let payload = read_payload(file_path.to_str().expect("test"));

        let config = ReadOnceConfig {
            enabled: true,
            mode: ReadOnceMode::Warn,
            ttl_seconds: 0, // Zero TTL means immediate expiry
        };

        // First read
        let result1 = handle_pre_tool_use_inner(&payload, Some("test-ttl"), &config, dir.path());
        assert!(result1.is_empty());

        // Second read beyond TTL (0 seconds) — passes through
        let result2 = handle_pre_tool_use_inner(&payload, Some("test-ttl"), &config, dir.path());
        assert!(result2.is_empty(), "expired TTL should pass through");
    }

    #[test]
    fn corrupt_cache_file_fail_soft() {
        let dir = TempDir::new().expect("test");
        let state_dir = dir.path();
        let ro_dir = read_once_state_dir(state_dir);
        fs::create_dir_all(&ro_dir).expect("test");
        fs::write(ro_dir.join("test-session.json"), b"not valid json{}").expect("test");

        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"content").expect("test");

        // Even with corrupt cache, the first read should pass through.
        // Uses the inner function with injectable state_dir so the corrupt file
        // in the TempDir is actually consulted.
        let result = handle_pre_tool_use_inner(
            &read_payload(file_path.to_str().expect("test")),
            Some("test-session"),
            &test_config_warn(),
            state_dir,
        );
        assert!(result.is_empty(), "corrupt cache should fail-soft");
    }

    #[test]
    fn session_cache_prune_stale_entries() {
        let state_dir = TempDir::new().expect("test");
        let ro_dir = read_once_state_dir(state_dir.path());
        fs::create_dir_all(&ro_dir).expect("test");

        let mut cache = SessionCache::new("test-prune");
        let now = unix_now();
        cache.entries.insert(
            "fresh_file".to_string(),
            ReadEntry {
                path: "fresh_path".to_string(),
                mtime_unix: 1000,
                first_read_at: now,
                hits: 0,
                tokens_saved: 0,
            },
        );
        cache.entries.insert(
            "stale_file".to_string(),
            ReadEntry {
                path: "stale_path".to_string(),
                mtime_unix: 1000,
                first_read_at: now - 3600, // 1 hour ago
                hits: 0,
                tokens_saved: 0,
            },
        );

        cache.prune(60); // 60 second TTL
        assert!(cache.entries.contains_key("fresh_file"));
        assert!(!cache.entries.contains_key("stale_file"));
    }

    #[test]
    fn session_cache_save_and_load_roundtrip() {
        let state_dir = TempDir::new().expect("test");
        let ro_dir = read_once_state_dir(state_dir.path());
        fs::create_dir_all(&ro_dir).expect("test");

        let mut cache = SessionCache::new("test-rt");
        cache.entries.insert(
            "/foo/bar.rs".to_string(),
            ReadEntry {
                path: "/foo/bar.rs".to_string(),
                mtime_unix: 12345,
                first_read_at: unix_now() - 100, // 100 seconds ago — well within 3600s TTL
                hits: 2,
                tokens_saved: 500,
            },
        );
        cache.save(state_dir.path()).expect("test");

        let loaded = SessionCache::load(state_dir.path(), "test-rt", 3600);
        assert_eq!(loaded.session_id, "test-rt");
        assert_eq!(loaded.entries.len(), 1);
        let entry = loaded.entries.get("/foo/bar.rs").expect("test");
        assert_eq!(entry.hits, 2);
        assert_eq!(entry.tokens_saved, 500);
    }

    // #792: ReadEntry and SessionCache derive Serialize/Deserialize and persist
    // as JSON. A serde roundtrip must be lossless — a drifted derive (renamed
    // field, wrong rename attr) would silently corrupt a user's session cache.
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_read_entry() -> impl Strategy<Value = ReadEntry> {
            (
                ".{0,40}",
                any::<i64>(),
                any::<i64>(),
                any::<u64>(),
                any::<u64>(),
            )
                .prop_map(|(path, mtime_unix, first_read_at, hits, tokens_saved)| {
                    ReadEntry {
                        path,
                        mtime_unix,
                        first_read_at,
                        hits,
                        tokens_saved,
                    }
                })
        }

        fn arb_session_cache() -> impl Strategy<Value = SessionCache> {
            (
                ".{0,20}",
                proptest::collection::hash_map(".{0,30}", arb_read_entry(), 0..5),
            )
                .prop_map(|(session_id, entries)| SessionCache {
                    session_id,
                    entries,
                })
        }

        proptest! {
            #[test]
            fn read_entry_json_roundtrips(entry in arb_read_entry()) {
                let json = serde_json::to_string(&entry).unwrap();
                let back: ReadEntry = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back, entry);
            }

            #[test]
            fn session_cache_json_roundtrips(cache in arb_session_cache()) {
                let json = serde_json::to_string(&cache).unwrap();
                let back: SessionCache = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back, cache);
            }
        }
    }
}
