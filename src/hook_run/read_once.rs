//! Read-once file deduplication hook (#318).
//!
//! Tracks which files have been read via `PreToolUse`/`Read` tool calls,
//! cached per-session in a flat JSON file under `state_dir/read_once/`.
//! Warns or denies redundant re-reads to save context-window tokens.
//!
//! Fail-soft: any cache/IO error logs to stderr and passes the read through
//! silently — the optimizer must never block real work.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::ReadOnce as ReadOnceConfig;
use crate::config::ReadOnceMode;
use crate::hook_run::session_state::unix_now;

use cache::within_ttl;
use state::cache_key;

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

mod cache;
mod stamp;
mod state;
pub use cache::SessionCache;
use stamp::file_stamp;
pub(in crate::hook_run) use state::session_cache_path;
pub use state::{clear_cache, read_once_state_dir};

/// Handle a PreToolUse event for the read-once feature.
///
/// Returns:
/// - Empty string → pass the read through (first read, changed file, partial
///   read, missing file, or any error).
/// - Advisory text → warn mode, the read is allowed but we emit an advisory.
/// - `"__DENY__:<reason>"` → deny mode, caller should emit a deny envelope.
///
/// Takes `state_dir` explicitly rather than resolving `crate::paths::state_dir()`
/// internally, so tests exercise this without touching the developer's real
/// state dir (#1089) and the sole production caller (`resolve_pre_tool_text`)
/// resolves it once for the whole PreToolUse decision.
pub(crate) fn handle_pre_tool_use(
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
        None => {
            // tool_input is required and always an object per Claude Code's
            // Read tool schema — this should never fire in practice. `error!`,
            // not `warn!`: the default `EnvFilter` (`RUST_LOG` unset) is
            // ERROR-only and drops anything weaker (#1133 precedent).
            tracing::error!("tool_input missing or not an object for Read PreToolUse payload");
            return String::new();
        }
    };
    // Extract file path — the real key is snake_case per Claude Code hook payload
    // convention, but accept PascalCase fallback for synthetic test payloads.
    let Some(file_path) = tool_input
        .get("file_path")
        .or_else(|| {
            let v = tool_input.get("filePath");
            if v.is_some() {
                // Hook stderr never reaches the agent, so the notice goes to the agent (#2818).
                crate::agent_notice::record(
                    "llmenv: read-once used the deprecated 'filePath' key instead of 'file_path'. \
                     Caching still applied. Fix: none is needed for caching. Report it if this \
                     notice repeats.",
                );
            }
            v
        })
        .and_then(|v| v.as_str())
    else {
        // file_path is required per Claude Code's Read tool schema — this
        // should never fire in practice. `error!`, not `warn!`/`debug!`: the
        // default `EnvFilter` (`RUST_LOG` unset) is ERROR-only and drops
        // anything weaker before it reaches a default-configured user's log
        // (#1133 precedent).
        tracing::error!("tool_input.file_path missing or not a string for Read PreToolUse payload");
        return String::new();
    };
    // Partial read bypass: if offset or limit are present, never cache
    if tool_input.contains_key("offset") || tool_input.contains_key("limit") {
        return String::new();
    }
    // Stat the file. Without a usable stamp the read passes through, not deduplicated (#3109).
    let path = Path::new(file_path);
    let Some((mtime, file_size)) = file_stamp(path) else {
        return String::new();
    };
    // Need a session id to cache
    let session_id = match session_id {
        Some(id) => id,
        None => return String::new(),
    };
    // `session_id` comes straight from the hook's stdin JSON, unsanitized.
    // Reject anything that isn't safe as a single path component before it
    // reaches `session_cache_path`'s `state_dir.join(...)` — a `../` or
    // absolute value would otherwise escape `state_dir` entirely (the
    // latter because `Path::join` discards the base on an absolute RHS).
    if !crate::paths::is_valid_short_name(session_id) {
        // A rejected session_id means the hook's own harness sent an unsafe
        // value — worth surfacing even though the rejection itself is safe.
        crate::agent_notice::record_logged(
            "llmenv: read-once rejected the session id from the hook input, because it is not a \
             safe file name. The Read call passed through without caching. Fix: report the \
             harness input that sent this session id.",
        );
        return String::new();
    }
    let path_key = cache_key(path);
    // Load session cache
    // Held from the load to the save, so a concurrent Read hook cannot overwrite the cache (#2678).
    let _lock = super::session_state::lock_state_file(
        &read_once_state_dir(state_dir),
        session_id,
        "read-once cache",
    );
    let mut cache = SessionCache::load(state_dir, session_id, config.ttl_seconds);
    let now = unix_now();
    let tokens_saved = file_size / 4;

    // Check existing entry
    let lookup_start = std::time::Instant::now();
    let extra = format!("path={file_path}");
    if let Some(entry) = cache.entries.get_mut(&path_key) {
        if entry.mtime_unix == mtime && within_ttl(now, entry.first_read_at, config.ttl_seconds) {
            // Cache hit within TTL — warn or deny.
            // Don't save on hit — hits/tokens_saved are advisory stats that
            // get accurately written on the next miss-path save.
            entry.hits = entry.hits.saturating_add(1);
            entry.tokens_saved = entry.tokens_saved.saturating_add(tokens_saved);
            crate::cache_trace::emit_cache_trace(
                "read_once",
                true,
                lookup_start.elapsed(),
                Some(&extra),
            );

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
    // Both branches above fall through here only on a miss — the hit branch
    // returns early — so one emit covers both (mtime/TTL miss and new entry).
    crate::cache_trace::emit_cache_trace("read_once", false, lookup_start.elapsed(), Some(&extra));

    if let Err(e) = cache.save(state_dir) {
        crate::agent_notice::record(format!(
            "llmenv: the read-once cache could not be saved: {e:#}. Fix: run `llmenv doctor` to \
             check the state directory."
        ));
    }

    String::new() // Pass through
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
