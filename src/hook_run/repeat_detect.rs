//! Repeat-loop detection (#1006), engine-neutral.
//!
//! Two independent trackers share one per-session state file
//! (`state_dir/repeat_detect/{session_id}.json`):
//!
//! - **`PreToolUse`**: tracks the most recent tool name + input per session.
//!   When N consecutive calls carry an identical signature, surfaces a
//!   warning telling the model to stop and reassess instead of letting it
//!   silently re-issue the same call forever — the failure mode observed on
//!   a small/local model that re-read the same file range for 5 turns with
//!   zero reasoning tokens.
//! - **`Stop`**: tracks the task-tracker's Stop-hook reminder text. The
//!   single most common real-world trigger for #1006 isn't an uncovered
//!   tool like `Bash` — it's the reminder itself re-firing identically every
//!   turn while a model reports (in prose, not via `llmenv task wait`) that
//!   it's blocked on something external. The reminder's own "don't stop
//!   mid-task" wording then forces action on every turn with no escape
//!   hatch, which is its own stuck loop. When the identical reminder repeats
//!   N times in a row, this appends a pointer to `llmenv task wait` instead
//!   of just repeating the same imperative forever.
//!
//! This lives in `hook_run` (not per-adapter) so it fires for any
//! adapter/model, mirroring how `task_tools.rs`'s redirect is shared by
//! every adapter rather than duplicated.
//!
//! Fail-soft: any cache/IO error logs to stderr and passes the call through
//! silently — the detector must never block real work.
//!
//! Load-modify-save has no lock: this assumes `PreToolUse` hook invocations
//! for a single session are never concurrent (each is a fresh subprocess
//! dispatched sequentially by the calling adapter). If that ever stops
//! holding, two concurrent calls could race and one increment could be
//! silently lost — add per-session locking if a future adapter changes this.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::RepeatDetect as RepeatDetectConfig;

/// Per-session state: the two trackers (`PreToolUse` and `Stop`) are
/// independent — a tool call between two Stop events must not reset the
/// Stop streak, and vice versa — so each gets its own signature/counter
/// pair in the same file. `#[serde(default)]` on the `stop_*` fields keeps
/// state files written before Stop-tracking existed loading cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionState {
    session_id: String,
    last_signature: Option<String>,
    consecutive: u32,
    #[serde(default)]
    last_stop_signature: Option<String>,
    #[serde(default)]
    stop_consecutive: u32,
}

impl SessionState {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            last_signature: None,
            consecutive: 0,
            last_stop_signature: None,
            stop_consecutive: 0,
        }
    }

    fn load(state_dir: &Path, session_id: &str) -> Self {
        let path = session_state_path(state_dir, session_id);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!("llmenv: failed to parse repeat-detect state: {e}");
                Self::new(session_id)
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(session_id),
            Err(e) => {
                eprintln!(
                    "llmenv: failed to read repeat-detect state {}: {e}",
                    path.display()
                );
                Self::new(session_id)
            }
        }
    }

    fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        let rd_dir = repeat_detect_state_dir(state_dir);
        // Same 7-day retention as `read_once`'s sibling cache — nothing here
        // needs a longer memory than that (the counter resets on any
        // different call, or expires with the session itself).
        super::session_state::prune_stale_json_files(&rd_dir, 7);
        let path = session_state_path(state_dir, &self.session_id);
        let json = serde_json::to_string(&self)?;
        crate::paths::write_owner_only_atomic(&path, json.as_bytes())?;
        Ok(())
    }
}

fn repeat_detect_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("repeat_detect")
}

fn session_state_path(state_dir: &Path, session_id: &str) -> PathBuf {
    repeat_detect_state_dir(state_dir).join(format!("{session_id}.json"))
}

/// Handle a `PreToolUse` event for the repeat-detect feature.
///
/// Returns an empty string to pass the call through, or a warning naming the
/// repeat count once `config.threshold` consecutive identical calls have
/// been observed. Never a `__DENY__:` decision — a false positive (e.g. a
/// deliberately re-run command) must never block real work, only nudge.
///
/// Takes `state_dir` explicitly rather than resolving `crate::paths::state_dir()`
/// internally, so tests exercise this without touching the developer's real
/// state dir (#1089) and the sole production caller (`resolve_pre_tool_text`)
/// resolves it once for the whole PreToolUse decision.
pub(crate) fn handle_pre_tool_use(
    stdin_payload: &serde_json::Value,
    session_id: Option<&str>,
    config: &RepeatDetectConfig,
    state_dir: &Path,
) -> String {
    let Some(session_id) = session_id else {
        return String::new();
    };
    // `session_id` comes straight from the hook's stdin JSON, unsanitized.
    // Reject anything that isn't safe as a single path component before it
    // ever reaches `session_state_path`'s `state_dir.join(...)` — a `../`
    // or absolute value would otherwise escape `state_dir` entirely (the
    // latter because `Path::join` discards the base on an absolute RHS).
    if !crate::paths::is_valid_short_name(session_id) {
        // A rejected session_id means the hook's own harness sent an unsafe
        // value -- worth surfacing even though the rejection itself is safe.
        // `error!`, not `warn!`: see the #1133 precedent below. Found during
        // #1209's pre-pr-review: read_once.rs's identical check already logs.
        tracing::error!("session_id failed path-safety validation for repeat_detect, rejecting");
        return String::new();
    }
    let Some(tool_name) = stdin_payload.get("tool_name").and_then(|v| v.as_str()) else {
        // tool_name is required on every PreToolUse payload -- this should
        // never fire in practice. `error!`, not `warn!`/`debug!`: the default
        // `EnvFilter` (`RUST_LOG` unset) is ERROR-only and drops anything
        // weaker before it reaches a default-configured user's log (#1133
        // precedent). Found during #1209's pre-pr-review as the same
        // "required field missing" category cd_guard/read_once already fix.
        tracing::error!("tool_name missing or not a string for PreToolUse payload");
        return String::new();
    };
    let tool_input = stdin_payload
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let signature = format!("{tool_name}:{tool_input}");

    let mut state = SessionState::load(state_dir, session_id);
    if state.last_signature.as_deref() == Some(signature.as_str()) {
        state.consecutive = state.consecutive.saturating_add(1);
    } else {
        state.last_signature = Some(signature);
        state.consecutive = 1;
    }
    let consecutive = state.consecutive;

    if let Err(e) = state.save(state_dir) {
        eprintln!("llmenv: failed to save repeat-detect state for session {session_id}: {e}");
    }

    // `threshold: 0` isn't rejected at config-load time (no validation
    // infrastructure exists for this kind of numeric feature field — same
    // gap `read_once`'s `ttl_seconds` has), so clamp here rather than warn
    // on literally every tool call.
    if consecutive >= config.threshold.max(1) {
        format!(
            "You've called {tool_name} with identical input {consecutive} times in a row. \
             This is very likely a stuck loop, not progress — stop, re-read the actual error or \
             goal, and try a different approach instead of repeating this call."
        )
    } else {
        String::new()
    }
}

/// Handle a `Stop` event's task-tracker reminder text under `state_dir`.
///
/// Returns `reminder` unchanged below `config.threshold` repeats; once the
/// identical reminder has fired that many times in a row for this session,
/// appends a pointer to `llmenv task wait` instead of just repeating the
/// same "keep working" imperative forever. Empty `reminder` passes through
/// untouched — nothing to track when there's no nag to begin with.
///
/// `state_dir` is caller-supplied, matching `handle_pre_tool_use` — resolving
/// it here instead would put every test of this path on the developer's real
/// state dir (#1109).
pub fn handle_stop(
    reminder: &str,
    session_id: Option<&str>,
    config: &RepeatDetectConfig,
    state_dir: &Path,
) -> String {
    if reminder.is_empty() {
        return String::new();
    }
    let Some(session_id) = session_id else {
        return reminder.to_string();
    };
    if !crate::paths::is_valid_short_name(session_id) {
        return reminder.to_string();
    }

    let mut state = SessionState::load(state_dir, session_id);
    if state.last_stop_signature.as_deref() == Some(reminder) {
        state.stop_consecutive = state.stop_consecutive.saturating_add(1);
    } else {
        state.last_stop_signature = Some(reminder.to_string());
        state.stop_consecutive = 1;
    }
    let consecutive = state.stop_consecutive;

    if let Err(e) = state.save(state_dir) {
        eprintln!("llmenv: failed to save repeat-detect state for session {session_id}: {e}");
    }

    if consecutive >= config.threshold.max(1) {
        format!(
            "{reminder}\n\nThis exact reminder has repeated {consecutive} times in a row with no \
             progress. If one of the listed tasks is your own and you're genuinely blocked on \
             something outside your control, run `llmenv task wait <slug> \"<reason>\"` — that \
             silences this nag until the blocker clears, instead of being told to \"keep \
             working\" every single turn. If none of the listed tasks are yours (a different, \
             possibly still-active session owns them), this repeat is expected and needs no \
             action from you — it clears on its own once that session updates or closes them."
        )
    } else {
        reminder.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn config(threshold: u32) -> RepeatDetectConfig {
        RepeatDetectConfig {
            enabled: true,
            threshold,
        }
    }

    fn bash_payload(cmd: &str) -> serde_json::Value {
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

    // ===== handle_stop tests =====

    const WIP_REMINDER: &str = "You still have task(s) in progress:\n- foo\nkeep working";

    #[test]
    fn empty_reminder_passes_through() {
        let dir = TempDir::new().expect("test");
        let out = handle_stop("", Some("s1"), &config(3), dir.path());
        assert!(out.is_empty());
    }

    #[test]
    fn no_session_id_passes_reminder_through_unchanged() {
        let dir = TempDir::new().expect("test");
        let out = handle_stop(WIP_REMINDER, None, &config(3), dir.path());
        assert_eq!(out, WIP_REMINDER);
    }

    #[test]
    fn below_threshold_reminder_unchanged() {
        let dir = TempDir::new().expect("test");
        for _ in 0..2 {
            let out = handle_stop(WIP_REMINDER, Some("s1"), &config(3), dir.path());
            assert_eq!(
                out, WIP_REMINDER,
                "below threshold must not append anything"
            );
        }
    }

    #[test]
    fn at_threshold_appends_task_wait_pointer() {
        let dir = TempDir::new().expect("test");
        for _ in 0..2 {
            handle_stop(WIP_REMINDER, Some("s1"), &config(3), dir.path());
        }
        let out = handle_stop(WIP_REMINDER, Some("s1"), &config(3), dir.path());
        assert!(
            out.starts_with(WIP_REMINDER),
            "original reminder must still be present"
        );
        assert!(
            out.contains("llmenv task wait"),
            "must point at the escape hatch: {out}"
        );
    }

    #[test]
    fn different_reminder_resets_stop_streak() {
        let dir = TempDir::new().expect("test");
        handle_stop(WIP_REMINDER, Some("s1"), &config(2), dir.path());
        let other = "You still have task(s) in progress:\n- bar\nkeep working";
        let out = handle_stop(other, Some("s1"), &config(2), dir.path());
        assert_eq!(out, other, "a different reminder must reset the streak");
    }

    #[test]
    fn pre_tool_use_streak_and_stop_streak_are_independent() {
        let dir = TempDir::new().expect("test");
        // Drive the PreToolUse tracker to just below its own threshold...
        handle_pre_tool_use(&bash_payload("ls"), Some("s1"), &config(2), dir.path());
        // ...then a Stop event with a fresh reminder must not be affected by
        // (or reset) the PreToolUse streak, and vice versa.
        let out = handle_stop(WIP_REMINDER, Some("s1"), &config(2), dir.path());
        assert_eq!(out, WIP_REMINDER);
        let tool_out = handle_pre_tool_use(&bash_payload("ls"), Some("s1"), &config(2), dir.path());
        assert!(
            !tool_out.is_empty(),
            "PreToolUse streak must have kept counting: {tool_out}"
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
                let out =
                    handle_pre_tool_use(&bash_payload("ls"), Some(evil), &config(1), dir.path());
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
        }
    }
}
