use super::*;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn the_counter_lock_is_taken_once_per_session() {
    let dir = tempfile::tempdir().unwrap();
    let held = counter_lock(dir.path(), "s1");
    assert!(held.is_some(), "a free lock must be taken");
    assert!(
        counter_lock(dir.path(), "s1").is_none(),
        "a held lock must not be taken twice"
    );
    drop(held);
}

pub(super) fn config(threshold: u32) -> RepeatDetectConfig {
    RepeatDetectConfig {
        enabled: true,
        threshold,
    }
}

pub(super) fn bash_payload(cmd: &str) -> serde_json::Value {
    json!({ "tool_name": "Bash", "tool_input": { "command": cmd } })
}

#[test]
fn no_session_id_passes_through() {
    let dir = TempDir::new().expect("test");
    let result = handle_pre_tool_use(&bash_payload("ls"), None, &config(3), dir.path());
    assert!(result.is_empty());
}

#[test]
fn zero_threshold_is_clamped_to_one_not_warn_every_call() {
    let dir = TempDir::new().expect("test");
    let out = handle_pre_tool_use(&bash_payload("ls"), Some("s1"), &config(0), dir.path());
    assert!(
        !out.is_empty(),
        "threshold 0 must clamp to 1, warning on the very first call"
    );
}

#[test]
fn unsafe_session_id_passes_through_without_escaping_state_dir() {
    let dir = TempDir::new().expect("test");
    // Wrapped in capture_logs (output discarded) even though this test
    // doesn't assert on logs: this hits the same tracing::error! callsite
    // as unsafe_session_id_logs_error below, and a sibling test reaching
    // that callsite outside any subscriber makes tracing's per-callsite
    // interest caching order-dependent across parallel test threads
    // (the exact hazard #1133's precedent flags) -- always running under
    // *some* subscriber keeps the callsite's interest live.
    crate::test_log_capture::capture_logs(|| {
        for evil in ["../../victim/pwn", "/tmp/llmenv-abs-escape", "..", "a/b"] {
            let out = handle_pre_tool_use(&bash_payload("ls"), Some(evil), &config(1), dir.path());
            assert!(
                out.is_empty(),
                "unsafe session_id {evil:?} must pass through: {out}"
            );
        }
    });
    // Nothing should have been written outside the state dir's repeat_detect/ subdir.
    assert!(!std::path::Path::new("/tmp/llmenv-abs-escape").exists());
}

// Found during #1209's pre-pr-review (security-audit): read_once.rs's
// identical check already logs this rejection; repeat_detect's own copy
// of the same check didn't.
#[test]
fn unsafe_session_id_logs_error() {
    let dir = TempDir::new().expect("test");
    let logs = crate::test_log_capture::capture_logs(|| {
        let out = handle_pre_tool_use(
            &bash_payload("ls"),
            Some("../escape"),
            &config(1),
            dir.path(),
        );
        assert!(out.is_empty());
    });
    assert!(
        logs.contains("session_id failed path-safety validation"),
        "expected an error log when session_id is rejected, got: {logs}"
    );
    assert!(
        logs.contains("ERROR"),
        "must log at error level, got: {logs}"
    );
}

#[test]
fn no_tool_name_passes_through() {
    let dir = TempDir::new().expect("test");
    // Wrapped in capture_logs (output discarded) for the same reason as
    // unsafe_session_id_passes_through_without_escaping_state_dir above --
    // avoids racing no_tool_name_logs_error below over the same callsite.
    crate::test_log_capture::capture_logs(|| {
        let result = handle_pre_tool_use(
            &json!({ "tool_input": {} }),
            Some("s1"),
            &config(3),
            dir.path(),
        );
        assert!(result.is_empty());
    });
}

// Found during #1209's pre-pr-review: the same "required field missing,
// should never happen" category cd_guard/read_once already log for.
#[test]
fn no_tool_name_logs_error() {
    let dir = TempDir::new().expect("test");
    let logs = crate::test_log_capture::capture_logs(|| {
        let result = handle_pre_tool_use(
            &json!({ "tool_input": {} }),
            Some("s1"),
            &config(3),
            dir.path(),
        );
        assert!(result.is_empty());
    });
    assert!(
        logs.contains("tool_name missing"),
        "expected an error log when tool_name is absent, got: {logs}"
    );
    assert!(
        logs.contains("ERROR"),
        "must log at error level, got: {logs}"
    );
}

#[test]
fn below_threshold_passes_through() {
    let dir = TempDir::new().expect("test");
    let payload = bash_payload("cargo test");
    for _ in 0..2 {
        let out = handle_pre_tool_use(&payload, Some("s1"), &config(3), dir.path());
        assert!(out.is_empty(), "should pass through below threshold");
    }
}

#[test]
fn at_threshold_warns() {
    let dir = TempDir::new().expect("test");
    let payload = bash_payload("sed -n '1,10p' foo.rs");
    for _ in 0..2 {
        handle_pre_tool_use(&payload, Some("s1"), &config(3), dir.path());
    }
    let out = handle_pre_tool_use(&payload, Some("s1"), &config(3), dir.path());
    assert!(!out.is_empty(), "3rd identical call should warn");
    assert!(!out.starts_with("__DENY__"), "must be advisory, not a deny");
    assert!(out.contains("Bash"));
    assert!(out.contains('3'));
}

#[test]
fn keeps_warning_past_threshold() {
    let dir = TempDir::new().expect("test");
    let payload = bash_payload("sed -n '1,10p' foo.rs");
    for _ in 0..5 {
        handle_pre_tool_use(&payload, Some("s1"), &config(3), dir.path());
    }
    let out = handle_pre_tool_use(&payload, Some("s1"), &config(3), dir.path());
    assert!(
        !out.is_empty(),
        "should keep warning on every call past threshold"
    );
    assert!(out.contains('6'), "count should keep climbing: {out}");
}

#[test]
fn different_input_resets_counter() {
    let dir = TempDir::new().expect("test");
    handle_pre_tool_use(&bash_payload("ls -la"), Some("s1"), &config(3), dir.path());
    handle_pre_tool_use(&bash_payload("ls -la"), Some("s1"), &config(3), dir.path());
    // Different command breaks the streak.
    let out = handle_pre_tool_use(&bash_payload("pwd"), Some("s1"), &config(3), dir.path());
    assert!(out.is_empty(), "different input must reset the streak");
}

#[test]
fn different_tool_name_resets_counter() {
    let dir = TempDir::new().expect("test");
    let input = json!({ "tool_name": "Bash", "tool_input": { "command": "x" } });
    let other = json!({ "tool_name": "Grep", "tool_input": { "command": "x" } });
    handle_pre_tool_use(&input, Some("s1"), &config(2), dir.path());
    let out = handle_pre_tool_use(&other, Some("s1"), &config(2), dir.path());
    assert!(out.is_empty(), "different tool name must reset the streak");
}

#[test]
fn separate_sessions_track_independently() {
    let dir = TempDir::new().expect("test");
    let payload = bash_payload("loop-cmd");
    handle_pre_tool_use(&payload, Some("s1"), &config(2), dir.path());
    // A different session's first call must not inherit s1's streak.
    let out = handle_pre_tool_use(&payload, Some("s2"), &config(2), dir.path());
    assert!(out.is_empty(), "sessions must not share state");
}

// #1196: save() must create its state directory owner-only, even though
// it no longer calls create_dir_all itself -- write_owner_only_atomic's
// own parent-directory creation must be the one and only path.
#[test]
fn a_state_file_cannot_set_the_read_failed_flag() {
    let state_dir = tempfile::tempdir().unwrap();
    let path = session_state_path(state_dir.path(), "forged");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"session_id":"forged","last_signature":null,"consecutive":0,"read_failed":true}"#,
    )
    .unwrap();
    assert!(!SessionState::load(state_dir.path(), "forged").read_failed);
}

#[cfg(unix)]
#[test]
fn save_keeps_a_state_file_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = tempfile::tempdir().unwrap();
    let path = session_state_path(state_dir.path(), "unread");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = r#"{ "session_id": "unread", "last_signature": null, "consecutive": 3 }"#;
    std::fs::write(&path, original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let state = SessionState::load(state_dir.path(), "unread");
    assert!(state.save(state_dir.path()).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn a_failed_save_is_an_agent_notice() {
    // A file where the state folder should be makes every read and write fail (#2833).
    let dir = TempDir::new().expect("test");
    std::fs::write(repeat_detect_state_dir(dir.path()), "").expect("test");
    handle_pre_tool_use(
        &bash_payload("ls"),
        Some("notice-2833"),
        &config(2),
        dir.path(),
    );
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("session notice-2833 could not be saved"),
        "{notices}"
    );
}

#[cfg(unix)]
#[test]
fn save_creates_state_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().expect("test");
    handle_pre_tool_use(&bash_payload("ls"), Some("s1"), &config(2), dir.path());

    let rd_dir = repeat_detect_state_dir(dir.path());
    let mode = std::fs::metadata(&rd_dir)
        .expect("test")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "repeat_detect dir must be owner-only, got {mode:o}"
    );
}

#[test]
fn corrupt_state_file_fail_soft() {
    let dir = TempDir::new().expect("test");
    let rd_dir = repeat_detect_state_dir(dir.path());
    std::fs::create_dir_all(&rd_dir).expect("test");
    std::fs::write(rd_dir.join("s1.json"), b"not valid json{}").expect("test");

    let out = handle_pre_tool_use(&bash_payload("ls"), Some("s1"), &config(3), dir.path());
    assert!(
        out.is_empty(),
        "corrupt state should fail-soft to a fresh streak"
    );
}

/// A state file that is ten days old, past the seven-day retention.
fn write_stale_state_file(rd_dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(rd_dir).expect("test");
    let path = rd_dir.join(name);
    std::fs::write(&path, b"{}").expect("test");
    let ten_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(ten_days_ago))
        .expect("test");
    path
}

#[test]
fn the_first_save_of_a_session_prunes_stale_state_files() {
    let dir = TempDir::new().expect("test");
    let stale = write_stale_state_file(&repeat_detect_state_dir(dir.path()), "old.json");

    SessionState::new("s1").save(dir.path()).expect("save");

    assert!(!stale.exists(), "the first save removes a stale file");
}

#[test]
fn a_later_save_of_the_same_session_does_not_scan_for_stale_files() {
    let dir = TempDir::new().expect("test");
    SessionState::new("s1")
        .save(dir.path())
        .expect("first save");
    let stale = write_stale_state_file(&repeat_detect_state_dir(dir.path()), "old.json");

    SessionState::new("s1")
        .save(dir.path())
        .expect("second save");

    assert!(stale.exists(), "only the first save of a session prunes");
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_session_state() -> impl Strategy<Value = SessionState> {
        (
            ".{0,20}",
            proptest::option::of(".{0,40}"),
            any::<u32>(),
            proptest::option::of(".{0,40}"),
            any::<u32>(),
        )
            .prop_map(
                |(
                    session_id,
                    last_signature,
                    consecutive,
                    last_stop_signature,
                    stop_consecutive,
                )| {
                    SessionState {
                        session_id,
                        last_signature,
                        consecutive,
                        last_stop_signature,
                        stop_consecutive,
                        read_failed: false,
                    }
                },
            )
    }

    proptest! {
        // #1006: SessionState derives Serialize/Deserialize and persists as
        // JSON. A serde roundtrip must be lossless — a drifted derive would
        // silently corrupt a session's repeat-detect streak.
        #[test]
        fn session_state_json_roundtrips(state in arb_session_state()) {
            let json = serde_json::to_string(&state).unwrap();
            let back: SessionState = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, state);
        }

        // The counter is saved without an fsync, so a power loss can leave a cut-off file.
        // Any strict prefix of a saved state, including the empty file, must load as a fresh
        // streak that can be saved again, not as a read failure that blocks the save.
        #[test]
        fn a_truncated_state_file_loads_as_a_fresh_streak(
            state in arb_session_state(),
            cut in 0.0..1.0f64,
        ) {
            let json = serde_json::to_string(&state).unwrap();
            let mut end = (json.len() as f64 * cut) as usize;
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            let dir = TempDir::new().unwrap();
            let path = session_state_path(dir.path(), "s1");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &json.as_bytes()[..end]).unwrap();
            let loaded = SessionState::load(dir.path(), "s1");
            prop_assert_eq!(loaded.consecutive, 0);
            prop_assert!(loaded.last_signature.is_none());
            prop_assert!(!loaded.read_failed);
        }
    }
}
