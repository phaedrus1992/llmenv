//! Tests for the read-once hook.

use super::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn prune_stale_sessions_removes_old_cache_and_orphan_lock_files() {
    let dir = tempfile::tempdir().expect("test");
    let cache_dir = read_once_state_dir(dir.path());
    std::fs::create_dir_all(&cache_dir).expect("test");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
    for name in ["old.json", "orphan.lock"] {
        let file = std::fs::File::create(cache_dir.join(name)).expect("test");
        file.set_modified(old).expect("test");
    }

    SessionCache::prune_stale_sessions(dir.path(), 7);

    assert!(!cache_dir.join("old.json").exists());
    assert!(!cache_dir.join("orphan.lock").exists());
}

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
fn a_deprecated_file_path_key_is_noticed_to_the_agent() {
    let dir = TempDir::new().expect("test");
    let payload = serde_json::json!({
        "tool_name": "Read",
        "tool_input": {"filePath": dir.path().join("a.txt").display().to_string()},
    });
    handle_pre_tool_use(
        &payload,
        Some("test-session"),
        &test_config_warn(),
        dir.path(),
    );
    let notice = crate::agent_notice::current().unwrap_or_default();
    assert!(notice.contains("deprecated 'filePath' key"), "{notice}");
    assert!(notice.contains("Fix:"), "{notice}");
}

#[test]
fn non_read_tool_passes_through() {
    let dir = TempDir::new().expect("test");
    let result = handle_pre_tool_use(
        &non_read_payload(),
        Some("test-session"),
        &test_config_warn(),
        dir.path(),
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
        dir.path(),
    );
    assert!(result.is_empty(), "partial read should pass through");
}

#[test]
fn missing_file_passes_through() {
    let dir = TempDir::new().expect("test");
    let result = handle_pre_tool_use(
        &read_payload("/nonexistent/file.txt"),
        Some("test-session"),
        &test_config_warn(),
        dir.path(),
    );
    assert!(result.is_empty(), "missing file should pass through");
}

#[test]
fn unsafe_session_id_passes_through_without_escaping_state_dir() {
    let dir = TempDir::new().expect("test");
    let file_path = dir.path().join("test.txt");
    fs::write(&file_path, b"content").expect("test");
    let payload = read_payload(file_path.to_str().expect("test"));

    // Wrapped in capture_logs (output discarded) even though this test
    // doesn't assert on logs: this hits the same tracing::error! callsite
    // as logs_error_when_session_id_fails_path_safety_check below, and a
    // sibling test reaching that callsite outside any subscriber makes
    // tracing's per-callsite interest caching order-dependent across
    // parallel test threads (the exact hazard #1133's precedent flags) --
    // always running under *some* subscriber keeps the callsite live.
    crate::test_log_capture::capture_logs(|| {
        for evil in ["../../victim/pwn", "/tmp/llmenv-abs-escape", "..", "a/b"] {
            let out = handle_pre_tool_use(&payload, Some(evil), &test_config_warn(), dir.path());
            assert!(
                out.is_empty(),
                "unsafe session_id {evil:?} must pass through: {out}"
            );
        }
    });
    assert!(!std::path::Path::new("/tmp/llmenv-abs-escape").exists());
}

// #1207: file_path is a required field per Claude Code's Read tool
// schema, so this should never fire in practice — but if it ever does,
// read_once would otherwise stop deduplicating with no trace in the logs.
#[test]
fn logs_error_when_file_path_missing() {
    let dir = TempDir::new().expect("test");
    let payload = serde_json::json!({ "tool_name": "Read", "tool_input": {} });
    let logs = crate::test_log_capture::capture_logs(|| {
        let result = handle_pre_tool_use(
            &payload,
            Some("test-session"),
            &test_config_warn(),
            dir.path(),
        );
        assert!(result.is_empty());
    });
    assert!(
        logs.contains("tool_input.file_path missing"),
        "expected an error log when tool_input.file_path is absent, got: {logs}"
    );
    assert!(
        logs.contains("ERROR"),
        "must log at error level so it survives the default (RUST_LOG-unset) filter, got: {logs}"
    );
}

// Found during #1209's pre-pr-review: the identical "required field
// missing, should never happen" category as file_path above, just one
// level up (tool_input itself missing/not-an-object) — was left unlogged.
#[test]
fn logs_error_when_tool_input_not_an_object() {
    let dir = TempDir::new().expect("test");
    let payload = serde_json::json!({ "tool_name": "Read", "tool_input": "not-an-object" });
    let logs = crate::test_log_capture::capture_logs(|| {
        let result = handle_pre_tool_use(
            &payload,
            Some("test-session"),
            &test_config_warn(),
            dir.path(),
        );
        assert!(result.is_empty());
    });
    assert!(
        logs.contains("tool_input missing or not an object"),
        "expected an error log when tool_input isn't an object, got: {logs}"
    );
    assert!(
        logs.contains("ERROR"),
        "must log at error level, got: {logs}"
    );
}

// Found during #1209's pre-pr-review: a rejected session_id means the
// harness sent an unsafe value -- a security-relevant rejection that was
// previously silent.
#[test]
fn logs_error_when_session_id_fails_path_safety_check() {
    let dir = TempDir::new().expect("test");
    let file_path = dir.path().join("test.txt");
    fs::write(&file_path, b"content").expect("test");
    let payload = read_payload(file_path.to_str().expect("test"));

    let logs = crate::test_log_capture::capture_logs(|| {
        let result =
            handle_pre_tool_use(&payload, Some("../escape"), &test_config_warn(), dir.path());
        assert!(result.is_empty());
    });
    assert!(
        logs.contains("rejected the session id"),
        "expected an error log when session_id is rejected, got: {logs}"
    );
    let notice = crate::agent_notice::current().unwrap_or_default();
    assert!(notice.contains("rejected the session id"), "{notice}");
    assert!(notice.contains("Fix:"), "{notice}");
    assert!(
        logs.contains("ERROR"),
        "must log at error level, got: {logs}"
    );
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
        dir.path(),
    );
    assert!(result.is_empty(), "no session id should pass through");
}

#[test]
fn first_read_passes_through() {
    let dir = TempDir::new().expect("test");
    let file_path = dir.path().join("test.txt");
    fs::write(&file_path, b"hello").expect("test");

    let result = handle_pre_tool_use(
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
    let result1 = handle_pre_tool_use(&payload, Some("test-warn"), &test_config_warn(), dir.path());
    assert!(result1.is_empty(), "first read should pass through");

    // Second read warns
    let result2 = handle_pre_tool_use(&payload, Some("test-warn"), &test_config_warn(), dir.path());
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
    let result1 = handle_pre_tool_use(&payload, Some("test-deny"), &test_config_deny(), dir.path());
    assert!(result1.is_empty(), "first read should pass through");

    // Second read denies
    let result2 = handle_pre_tool_use(&payload, Some("test-deny"), &test_config_deny(), dir.path());
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
    let result1 = handle_pre_tool_use(
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
    let result2 = handle_pre_tool_use(
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
    let result1 = handle_pre_tool_use(&payload, Some("test-ttl"), &config, dir.path());
    assert!(result1.is_empty());

    // Second read beyond TTL (0 seconds) — passes through
    let result2 = handle_pre_tool_use(&payload, Some("test-ttl"), &config, dir.path());
    assert!(result2.is_empty(), "expired TTL should pass through");
}

#[test]
fn a_cache_file_cannot_set_the_read_failed_flag() {
    let state_dir = tempfile::tempdir().unwrap();
    let path = session_cache_path(state_dir.path(), "forged");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"session_id":"forged","entries":{},"read_failed":true}"#,
    )
    .unwrap();
    assert!(!SessionCache::load(state_dir.path(), "forged", 3600).read_failed);
}

#[test]
fn save_keeps_a_cache_file_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = tempfile::tempdir().unwrap();
    let path = session_cache_path(state_dir.path(), "unread");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = r#"{ "session_id": "unread", "entries": {} }"#;
    std::fs::write(&path, original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let logs = crate::test_log_capture::capture_logs(|| {
        let cache = SessionCache::load(state_dir.path(), "unread", 3600);
        cache.save(state_dir.path()).unwrap();
    });
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(logs.contains("not saved"), "{logs}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
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
    let result = handle_pre_tool_use(
        &read_payload(file_path.to_str().expect("test")),
        Some("test-session"),
        &test_config_warn(),
        state_dir,
    );
    assert!(result.is_empty(), "corrupt cache should fail-soft");
}

mod cache;

#[test]
fn the_advisory_reports_a_quarter_of_the_file_size_as_tokens_saved() {
    let dir = TempDir::new().expect("test");
    let file_path = dir.path().join("four-hundred.txt");
    fs::write(&file_path, vec![b'a'; 400]).expect("test");
    let payload = read_payload(file_path.to_str().expect("test"));
    let _first = handle_pre_tool_use(
        &payload,
        Some("test-tokens"),
        &test_config_warn(),
        dir.path(),
    );
    let second = handle_pre_tool_use(
        &payload,
        Some("test-tokens"),
        &test_config_warn(),
        dir.path(),
    );
    assert!(
        second.contains("~100 tokens saved"),
        "a 400-byte file saves 100 tokens: {second}"
    );
}
