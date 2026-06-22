//! Stable auth cache for materialized Claude Code folders.
//!
//! `CLAUDE_CONFIG_DIR` changes on every content-hash change, so any auth state
//! stored inside it is lost on re-render. This module caches the most-recently-
//! seen `oauthAccount` blob in a stable location that survives hash changes, and
//! injects it into newly-materialized folders so the user stays logged in.
//!
//! **Cache layout**: `<adapter_root>/state/auth/<uuid>.json`
//! Each file holds one [`AuthEntry`] serialized as JSON (owner-only, 0o600).

pub mod detect;

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Top-level key in `.claude.json` that carries OAuth tokens.
const OAUTH_ACCOUNT_KEY: &str = "oauthAccount";
/// `.claude.json` path relative to `CLAUDE_CONFIG_DIR`.
const CLAUDE_JSON_FILE: &str = ".claude.json";
/// Subdirectory under the adapter's stable state dir for cached auth files.
const AUTH_SUBDIR: &str = "auth";

/// A cached OAuth entry extracted from `.claude.json`'s `oauthAccount` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthEntry {
    /// Stable per-account UUID (`oauthAccount.id`). Used as the cache filename.
    pub uuid: String,
    /// Email address for human-readable stderr notifications.
    pub email: String,
    /// RFC 3339 timestamp of when this entry was last observed or refreshed.
    pub last_seen: String,
    /// Full `oauthAccount` JSON blob, injected verbatim into new folders.
    pub raw: serde_json::Value,
}

/// Auth cache directory: `<adapter_root>/state/auth/`.
#[must_use]
pub fn auth_cache_dir(adapter_root: &Path) -> PathBuf {
    crate::materialize::state::state_dir(adapter_root).join(AUTH_SUBDIR)
}

/// Cache file for a specific UUID: `<auth_cache_dir>/<uuid>.json`.
///
/// # Errors
/// Returns an error when `uuid` contains path-traversal or absolute components,
/// or is not a valid UUID-like string.
pub fn auth_entry_path(adapter_root: &Path, uuid: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        is_safe_uuid(uuid),
        "auth UUID contains unsafe characters and cannot be used as a filename: {uuid}"
    );
    Ok(auth_cache_dir(adapter_root).join(format!("{uuid}.json")))
}

/// Extract an [`AuthEntry`] from a `.claude.json`-shaped JSON document.
///
/// Returns `None` when `oauthAccount` is absent or missing required fields.
#[must_use]
pub fn extract_auth_entry(doc: &serde_json::Value) -> Option<AuthEntry> {
    let account = doc.get(OAUTH_ACCOUNT_KEY)?;
    let uuid = account.get("id")?.as_str()?.to_owned();
    if uuid.is_empty() {
        return None;
    }
    let email = account
        .get("emailAddress")
        .or_else(|| account.get("email"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    Some(AuthEntry {
        uuid,
        email,
        last_seen: rfc3339_now(),
        raw: account.clone(),
    })
}

/// Read the auth entry from `config_dir/.claude.json`, if present and parseable.
///
/// Returns `None` when the file is absent or has no `oauthAccount` block.
///
/// # Errors
/// Returns an error when the file exists but cannot be read or is not valid JSON.
pub fn read_auth_from_dir(config_dir: &Path) -> anyhow::Result<Option<AuthEntry>> {
    let path = config_dir.join(CLAUDE_JSON_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    };
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} for auth extraction", path.display()))?;
    Ok(extract_auth_entry(&doc))
}

/// Write (or refresh) an [`AuthEntry`] to the stable auth cache.
///
/// Uses `write_owner_only_atomic` — 0o600, atomic rename.
///
/// # Errors
/// Returns an error when the UUID is unsafe or the write fails.
pub fn save_auth_entry(adapter_root: &Path, entry: &AuthEntry) -> anyhow::Result<()> {
    let path = auth_entry_path(adapter_root, &entry.uuid)?;
    let json = serde_json::to_string_pretty(entry)?;
    crate::paths::write_owner_only_atomic(&path, json.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing auth cache {}: {e}", path.display()))
}

/// Load all valid [`AuthEntry`] files from the cache, sorted newest-first.
///
/// Unreadable or unparseable cache files are skipped — stale entries from a
/// prior crashed write must not block normal operation.
///
/// # Errors
/// Returns an error only on I/O failure reading the cache directory itself.
pub fn load_all_auth_entries(adapter_root: &Path) -> anyhow::Result<Vec<AuthEntry>> {
    let dir = auth_cache_dir(adapter_root);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading auth cache dir {}: {e}",
                dir.display()
            ));
        }
    };
    let mut entries: Vec<AuthEntry> = read_dir
        .filter_map(|res| {
            let entry = res.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let bytes = std::fs::read(&path).ok()?;
            serde_json::from_slice::<AuthEntry>(&bytes).ok()
        })
        .collect();
    entries.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    Ok(entries)
}

/// Choose the best cached auth entry to inherit into a new folder.
///
/// Selects the most-recently-seen entry. When multiple entries exist, logs
/// the others so users know alternate accounts are cached.
///
/// # Errors
/// Returns an error on I/O failure reading the cache directory.
pub fn choose_auth_for_inheritance(adapter_root: &Path) -> anyhow::Result<Option<AuthEntry>> {
    let entries = load_all_auth_entries(adapter_root)?;
    match entries.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        [best, rest @ ..] => {
            for other in rest {
                tracing::debug!(
                    "auth cache: also have {} ({}); use `llmenv login` to switch",
                    other.email,
                    other.uuid
                );
            }
            Ok(Some(best.clone()))
        }
    }
}

/// Upsert the `oauthAccount` from `entry` into `<config_dir>/.claude.json`.
///
/// Follows the same read-merge-write discipline as `merge_mcp_into_claude_json`:
/// reads existing file (absent → `{}`), corrupt → hard error, upserts only the
/// `oauthAccount` key, writes back with `write_owner_only_atomic`. All other
/// keys (`mcpServers`, `numStartups`, project state) are preserved verbatim.
///
/// # Errors
/// Returns an error when `.claude.json` is corrupt or the atomic write fails.
pub fn inject_auth_into_claude_json(config_dir: &Path, entry: &AuthEntry) -> anyhow::Result<()> {
    let path = config_dir.join(CLAUDE_JSON_FILE);
    let mut doc = read_claude_json_for_inject(&path)?;
    let Some(obj) = doc.as_object_mut() else {
        anyhow::bail!(
            "existing {} is not a JSON object; refusing to overwrite (would destroy Claude \
             state). Fix or remove the file and re-run.",
            path.display()
        );
    };
    obj.insert(OAUTH_ACCOUNT_KEY.to_string(), entry.raw.clone());
    let json = serde_json::to_string_pretty(&doc)?;
    crate::paths::write_owner_only_atomic(&path, json.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))
}

/// Read `.claude.json`, returning `{}` when absent. Corrupt = hard error.
fn read_claude_json_for_inject(path: &Path) -> anyhow::Result<serde_json::Value> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "existing {} is not valid JSON; refusing to overwrite (would destroy Claude \
                 state). Fix or remove the file and re-run.",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::Value::Object(serde_json::Map::new()))
        }
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// True when `uuid` is safe as a filename: non-empty, UUID hex+hyphens only,
/// no path separators, no `..`, not absolute. Defends against a crafted
/// `.claude.json` that could otherwise write a cache file to an arbitrary path.
fn is_safe_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && !crate::paths::is_unsafe_join_target(uuid)
        && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Current time as a minimal RFC 3339 UTC string (`YYYY-MM-DDTHH:MM:SSZ`).
/// No external date crate required — only used for cache-entry sorting.
fn rfc3339_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day, hour, min, sec) = secs_to_datetime(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn secs_to_datetime(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;
    let (year, month, day) = days_to_ymd(secs / 86400);
    (year, month, day, hour, min, sec)
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970u64;
    loop {
        let in_year = if is_leap(year) { 366 } else { 365 };
        if days < in_year {
            break;
        }
        days -= in_year;
        year += 1;
    }
    let months = if is_leap(year) {
        [31u64, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31u64, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u64;
    for &mdays in &months {
        if days < mdays {
            break;
        }
        days -= mdays;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(year: u64) -> bool {
    year.is_multiple_of(4) && !year.is_multiple_of(100) || year.is_multiple_of(400)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample_claude_json(uuid: &str, email: &str) -> serde_json::Value {
        serde_json::json!({
            "oauthAccount": {
                "id": uuid,
                "emailAddress": email,
                "displayName": "Test User"
            },
            "mcpServers": {},
            "numStartups": 3
        })
    }

    #[test]
    fn extract_auth_entry_happy_path() {
        let doc = sample_claude_json("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "user@example.com");
        let entry = extract_auth_entry(&doc).unwrap();
        assert_eq!(entry.uuid, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(entry.email, "user@example.com");
        assert_eq!(entry.raw["id"], "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    }

    #[test]
    fn extract_auth_entry_absent_key() {
        let doc = serde_json::json!({ "mcpServers": {} });
        assert!(extract_auth_entry(&doc).is_none());
    }

    #[test]
    fn extract_auth_entry_missing_uuid() {
        let doc = serde_json::json!({ "oauthAccount": { "emailAddress": "x@y.com" } });
        assert!(extract_auth_entry(&doc).is_none());
    }

    #[test]
    fn extract_auth_entry_empty_uuid() {
        let doc = serde_json::json!({ "oauthAccount": { "id": "" } });
        assert!(extract_auth_entry(&doc).is_none());
    }

    #[test]
    fn extract_auth_entry_fallback_email_key() {
        // Some Claude Code versions use "email" instead of "emailAddress".
        let doc = serde_json::json!({
            "oauthAccount": { "id": "aaaa", "email": "alt@example.com" }
        });
        let entry = extract_auth_entry(&doc).unwrap();
        assert_eq!(entry.email, "alt@example.com");
    }

    #[test]
    fn is_safe_uuid_accepts_standard_uuid() {
        assert!(is_safe_uuid("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn is_safe_uuid_rejects_path_traversal() {
        assert!(!is_safe_uuid("../../etc/passwd"));
        assert!(!is_safe_uuid("/absolute/path"));
    }

    #[test]
    fn is_safe_uuid_rejects_non_hex() {
        assert!(!is_safe_uuid("uuid with spaces"));
        assert!(!is_safe_uuid("uuid/slash"));
    }

    #[test]
    fn is_safe_uuid_rejects_empty() {
        assert!(!is_safe_uuid(""));
    }

    #[test]
    fn inject_auth_preserves_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude.json");
        let initial = serde_json::json!({
            "mcpServers": { "foo": { "command": "bar" } },
            "numStartups": 5
        });
        std::fs::write(&path, serde_json::to_string(&initial).unwrap()).unwrap();

        let doc = sample_claude_json("aaaa1111-0000-0000-0000-000000000000", "test@test.com");
        let entry = extract_auth_entry(&doc).unwrap();
        inject_auth_into_claude_json(tmp.path(), &entry).unwrap();

        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // Auth injected.
        assert_eq!(
            result["oauthAccount"]["id"],
            "aaaa1111-0000-0000-0000-000000000000"
        );
        // Existing keys preserved.
        assert_eq!(result["numStartups"], 5);
        assert!(result["mcpServers"]["foo"].is_object());
    }

    #[test]
    fn inject_auth_into_absent_file_creates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = sample_claude_json("bbbb2222-0000-0000-0000-000000000000", "new@test.com");
        let entry = extract_auth_entry(&doc).unwrap();
        inject_auth_into_claude_json(tmp.path(), &entry).unwrap();

        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            result["oauthAccount"]["id"],
            "bbbb2222-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = AuthEntry {
            uuid: "cccc3333-0000-0000-0000-000000000000".to_string(),
            email: "round@trip.com".to_string(),
            last_seen: "2025-01-01T00:00:00Z".to_string(),
            raw: serde_json::json!({"id": "cccc3333-0000-0000-0000-000000000000"}),
        };
        save_auth_entry(tmp.path(), &entry).unwrap();
        let entries = load_all_auth_entries(tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uuid, entry.uuid);
        assert_eq!(entries[0].email, entry.email);
    }

    #[test]
    fn load_all_returns_empty_when_no_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = load_all_auth_entries(tmp.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn choose_auth_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(choose_auth_for_inheritance(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn choose_auth_returns_most_recent() {
        let tmp = tempfile::tempdir().unwrap();
        for (uuid, last_seen) in [
            (
                "aaaa0000-0000-0000-0000-000000000000",
                "2025-01-01T00:00:00Z",
            ),
            (
                "bbbb0000-0000-0000-0000-000000000000",
                "2025-06-01T00:00:00Z",
            ),
        ] {
            let entry = AuthEntry {
                uuid: uuid.to_string(),
                email: format!("{uuid}@test.com"),
                last_seen: last_seen.to_string(),
                raw: serde_json::json!({"id": uuid}),
            };
            save_auth_entry(tmp.path(), &entry).unwrap();
        }
        let chosen = choose_auth_for_inheritance(tmp.path()).unwrap().unwrap();
        // June 2025 sorts after January 2025 lexicographically.
        assert_eq!(chosen.uuid, "bbbb0000-0000-0000-0000-000000000000");
    }

    #[test]
    fn rfc3339_now_format() {
        let ts = rfc3339_now();
        // Must match YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "unexpected timestamp length: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert!(ts.ends_with('Z'));
    }

    use proptest::prelude::*;

    proptest! {
        // ponytail: bounded to year ~2100; days_to_ymd loops year-by-year from 1970, unbounded u64 never terminates
        #[test]
        fn prop_secs_roundtrip(secs in 0u64..=4_102_444_800u64) {
            let (year, month, day, hour, min, sec) = secs_to_datetime(secs);
            prop_assert!((1970..=2100).contains(&year), "year out of range: {}", year);
            prop_assert!((1..=12).contains(&month), "month out of range: {}", month);
            prop_assert!((1..=31).contains(&day), "day out of range: {}", day);
            prop_assert!(hour < 24, "hour out of range: {}", hour);
            prop_assert!(min < 60, "minute out of range: {}", min);
            prop_assert!(sec < 60, "second out of range: {}", sec);
        }

        #[test]
        // ponytail: 50000 days ≈ year 2106, consistent with secs upper bound (~47481 days)
        fn prop_days_to_ymd_valid_ranges(days in 0u64..=50000) {
            let (year, month, day) = days_to_ymd(days);
            prop_assert!(year >= 1970, "year before epoch: {}", year);
            prop_assert!((1..=12).contains(&month), "month out of range: {}", month);
            prop_assert!((1..=31).contains(&day), "day out of range: {}", day);
        }

        #[test]
        fn prop_is_safe_uuid_accepts_valid_hex(s in "[0-9a-fA-F-]+") {
            prop_assert!(is_safe_uuid(&s), "expected valid hex UUID to be accepted: {}", s);
        }

        #[test]
        fn prop_is_safe_uuid_rejects_unsafe_paths(
            s in r"[a-zA-Z0-9\-]*(\.\.|\.|/|\\|:|\x00)[a-zA-Z0-9\-]*"
        ) {
            prop_assert!(!is_safe_uuid(&s), "expected path-unsafe UUID to be rejected: {:?}", s);
        }
    }
}
