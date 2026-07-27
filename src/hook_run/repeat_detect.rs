//! Repeat-tool-call loop detection (#1006), engine-neutral.
//!
//! Tracks the most recent `PreToolUse` tool name + input per session, cached
//! under `state_dir/repeat_detect/{session_id}.json`. When N consecutive
//! calls carry an identical signature, surfaces a warning telling the model
//! to stop and reassess instead of letting it silently re-issue the same
//! call forever — the failure mode observed on a small/local model (#1006)
//! that re-read the same file range for 5 turns with zero reasoning tokens.
//!
//! This lives in `hook_run` (not per-adapter) so it fires for any
//! adapter/model, mirroring how `task_tools.rs`'s redirect is shared by
//! every adapter rather than duplicated.
//!
//! Fail-soft: any cache/IO error logs to stderr and passes the call through
//! silently — the detector must never block real work.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::RepeatDetect as RepeatDetectConfig;

/// Per-session state: only the most recent call signature and how many
/// consecutive times it's repeated need to survive across hook invocations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionState {
    session_id: String,
    last_signature: Option<String>,
    consecutive: u32,
}

impl SessionState {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            last_signature: None,
            consecutive: 0,
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
        std::fs::create_dir_all(&rd_dir)?;
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
pub fn handle_pre_tool_use(
    stdin_payload: &serde_json::Value,
    session_id: Option<&str>,
    config: &RepeatDetectConfig,
) -> String {
    let Ok(state_dir) = crate::paths::state_dir().inspect_err(|e| {
        tracing::warn!("failed to resolve state_dir for repeat-detect pre-tool-use: {e}")
    }) else {
        return String::new();
    };
    handle_pre_tool_use_inner(stdin_payload, session_id, config, &state_dir)
}

/// Like [`handle_pre_tool_use`] but with an injectable `state_dir` for testing.
fn handle_pre_tool_use_inner(
    stdin_payload: &serde_json::Value,
    session_id: Option<&str>,
    config: &RepeatDetectConfig,
    state_dir: &Path,
) -> String {
    let Some(session_id) = session_id else {
        return String::new();
    };
    let Some(tool_name) = stdin_payload.get("tool_name").and_then(|v| v.as_str()) else {
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
        eprintln!("llmenv: failed to save repeat-detect state: {e}");
    }

    if consecutive >= config.threshold {
        format!(
            "You've called {tool_name} with identical input {consecutive} times in a row. \
             This is very likely a stuck loop, not progress — stop, re-read the actual error or \
             goal, and try a different approach instead of repeating this call."
        )
    } else {
        String::new()
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
        let result = handle_pre_tool_use(&bash_payload("ls"), None, &config(3));
        assert!(result.is_empty());
        drop(dir);
    }

    #[test]
    fn no_tool_name_passes_through() {
        let dir = TempDir::new().expect("test");
        let result = handle_pre_tool_use_inner(
            &json!({ "tool_input": {} }),
            Some("s1"),
            &config(3),
            dir.path(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn below_threshold_passes_through() {
        let dir = TempDir::new().expect("test");
        let payload = bash_payload("cargo test");
        for _ in 0..2 {
            let out = handle_pre_tool_use_inner(&payload, Some("s1"), &config(3), dir.path());
            assert!(out.is_empty(), "should pass through below threshold");
        }
    }

    #[test]
    fn at_threshold_warns() {
        let dir = TempDir::new().expect("test");
        let payload = bash_payload("sed -n '1,10p' foo.rs");
        for _ in 0..2 {
            handle_pre_tool_use_inner(&payload, Some("s1"), &config(3), dir.path());
        }
        let out = handle_pre_tool_use_inner(&payload, Some("s1"), &config(3), dir.path());
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
            handle_pre_tool_use_inner(&payload, Some("s1"), &config(3), dir.path());
        }
        let out = handle_pre_tool_use_inner(&payload, Some("s1"), &config(3), dir.path());
        assert!(
            !out.is_empty(),
            "should keep warning on every call past threshold"
        );
        assert!(out.contains('6'), "count should keep climbing: {out}");
    }

    #[test]
    fn different_input_resets_counter() {
        let dir = TempDir::new().expect("test");
        handle_pre_tool_use_inner(&bash_payload("ls -la"), Some("s1"), &config(3), dir.path());
        handle_pre_tool_use_inner(&bash_payload("ls -la"), Some("s1"), &config(3), dir.path());
        // Different command breaks the streak.
        let out =
            handle_pre_tool_use_inner(&bash_payload("pwd"), Some("s1"), &config(3), dir.path());
        assert!(out.is_empty(), "different input must reset the streak");
    }

    #[test]
    fn different_tool_name_resets_counter() {
        let dir = TempDir::new().expect("test");
        let input = json!({ "tool_name": "Bash", "tool_input": { "command": "x" } });
        let other = json!({ "tool_name": "Grep", "tool_input": { "command": "x" } });
        handle_pre_tool_use_inner(&input, Some("s1"), &config(2), dir.path());
        let out = handle_pre_tool_use_inner(&other, Some("s1"), &config(2), dir.path());
        assert!(out.is_empty(), "different tool name must reset the streak");
    }

    #[test]
    fn separate_sessions_track_independently() {
        let dir = TempDir::new().expect("test");
        let payload = bash_payload("loop-cmd");
        handle_pre_tool_use_inner(&payload, Some("s1"), &config(2), dir.path());
        // A different session's first call must not inherit s1's streak.
        let out = handle_pre_tool_use_inner(&payload, Some("s2"), &config(2), dir.path());
        assert!(out.is_empty(), "sessions must not share state");
    }

    #[test]
    fn corrupt_state_file_fail_soft() {
        let dir = TempDir::new().expect("test");
        let rd_dir = repeat_detect_state_dir(dir.path());
        std::fs::create_dir_all(&rd_dir).expect("test");
        std::fs::write(rd_dir.join("s1.json"), b"not valid json{}").expect("test");

        let out =
            handle_pre_tool_use_inner(&bash_payload("ls"), Some("s1"), &config(3), dir.path());
        assert!(
            out.is_empty(),
            "corrupt state should fail-soft to a fresh streak"
        );
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_session_state() -> impl Strategy<Value = SessionState> {
            (".{0,20}", proptest::option::of(".{0,40}"), any::<u32>()).prop_map(
                |(session_id, last_signature, consecutive)| SessionState {
                    session_id,
                    last_signature,
                    consecutive,
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
