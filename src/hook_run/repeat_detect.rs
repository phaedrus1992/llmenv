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
//!   of just repeating the same imperative forever — and past
//!   [`SILENCE_AFTER_THRESHOLD_MULTIPLE`] times that, stops repeating it at
//!   all (#1247): the listed tasks are often none of the current session's
//!   own (a different, still-active session's), so the `task wait` pointer
//!   is moot advice and the reminder had become the loop, not the fix.
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

/// Once the identical Stop reminder has repeated this many multiples of
/// `config.threshold`, stop repeating it entirely instead of escalating the
/// wording again (#1247). Below this, the `llmenv task wait` pointer added
/// at `threshold` is real, actionable advice; past it, the agent has had
/// every opportunity to act on it, and further identical repeats add no new
/// information — becoming the exact "stuck loop with no escape hatch"
/// pattern this whole module exists to break, just self-inflicted by the
/// reminder itself instead of a tool call. A different reminder (the task
/// set changed) resets the streak and re-arms this normally.
const SILENCE_AFTER_THRESHOLD_MULTIPLE: u32 = 3;

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
    /// Set when the state file exists but cannot be read. A save then would replace the
    /// unread file with a fresh counter, so `save` skips the write (#2721).
    #[serde(skip)]
    read_failed: bool,
}

impl SessionState {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            last_signature: None,
            consecutive: 0,
            last_stop_signature: None,
            stop_consecutive: 0,
            read_failed: false,
        }
    }

    fn load(state_dir: &Path, session_id: &str) -> Self {
        let path = session_state_path(state_dir, session_id);
        match crate::paths::bounded::state_text_io(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::error!(
                    "failed to parse repeat-detect state {}, starting fresh: {e}",
                    path.display()
                );
                Self::new(session_id)
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(session_id),
            Err(e) => {
                tracing::error!(
                    "failed to read repeat-detect state {}, saves are skipped: {e}",
                    path.display()
                );
                Self {
                    read_failed: true,
                    ..Self::new(session_id)
                }
            }
        }
    }

    fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        // A write after a failed read would replace good state with this empty one.
        anyhow::ensure!(
            !self.read_failed,
            "the state file could not be read, so it was kept as it is"
        );
        let rd_dir = repeat_detect_state_dir(state_dir);
        let path = session_state_path(state_dir, &self.session_id);
        // Same 7-day retention as `read_once`'s sibling cache — nothing here
        // needs a longer memory than that (the counter resets on any
        // different call, or expires with the session itself). The two directory scans run
        // once per session, on its first save, not on every tool call.
        if !path.exists() {
            super::session_state::prune_stale_json_files(&rd_dir, 7);
            super::session_state::prune_orphan_locks(&rd_dir, 7);
        }
        let json = serde_json::to_string(&self)?;
        crate::paths::write_owner_only_atomic_unsynced(&path, json.as_bytes())?;
        Ok(())
    }
}

fn repeat_detect_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("repeat_detect")
}

/// Lock for one read-change-save of the session's counter file (#2678). When the lock fails,
/// `lock_state_file` logs it and the update runs unlocked, as it did before the lock existed.
fn counter_lock(state_dir: &Path, session_id: &str) -> Option<std::fs::File> {
    super::session_state::lock_state_file(
        &repeat_detect_state_dir(state_dir),
        session_id,
        "repeat detect",
    )
}

pub(super) fn session_state_path(state_dir: &Path, session_id: &str) -> PathBuf {
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
    let state_key = super::session_state::agent_state_key(session_id, stdin_payload);
    let tool_input = stdin_payload
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let signature = format!("{tool_name}:{tool_input}");

    let _lock = counter_lock(state_dir, &state_key);
    let mut state = SessionState::load(state_dir, &state_key);
    if state.last_signature.as_deref() == Some(signature.as_str()) {
        state.consecutive = state.consecutive.saturating_add(1);
    } else {
        state.last_signature = Some(signature);
        state.consecutive = 1;
    }
    let consecutive = state.consecutive;

    if let Err(e) = state.save(state_dir) {
        note_save_failure(&state_key, &e);
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

    let _lock = counter_lock(state_dir, session_id);
    let mut state = SessionState::load(state_dir, session_id);
    if state.last_stop_signature.as_deref() == Some(reminder) {
        state.stop_consecutive = state.stop_consecutive.saturating_add(1);
    } else {
        state.last_stop_signature = Some(reminder.to_string());
        state.stop_consecutive = 1;
    }
    let consecutive = state.stop_consecutive;

    if let Err(e) = state.save(state_dir) {
        note_save_failure(session_id, &e);
    }

    let threshold = config.threshold.max(1);
    if consecutive >= threshold.saturating_mul(SILENCE_AFTER_THRESHOLD_MULTIPLE) {
        // Past the point where the escalated message below already gave the
        // agent every chance to act — repeating it further is the loop this
        // module exists to break, not a fix for one. Silent until the
        // underlying task set changes (a different reminder resets the
        // streak) or the session ends.
        String::new()
    } else if consecutive >= threshold {
        format!(
            "{reminder}\n\nThis exact reminder has repeated {consecutive} times in a row with no \
             progress. If a listed task is your own, run `llmenv task start <slug>` to work on \
             it, or `llmenv task wait <slug> \"<reason>\"` if you are blocked on something \
             outside your control — that silences this nag until the blocker clears. If none \
             of the listed tasks are yours (a different, possibly still-active session owns \
             them), this repeat is expected and needs no action from you — it clears on its \
             own once that session updates or closes them."
        )
    } else {
        reminder.to_string()
    }
}

/// Logs a failed save and tells the agent. The engine drops a hook's stderr (#2833).
fn note_save_failure(session_id: &str, err: &impl std::fmt::Display) {
    crate::agent_notice::record_logged(format!(
        "llmenv: the repeat-call counter of session {session_id} could not be saved ({err}), so \
         a repeated call may not be flagged. Fix: run `llmenv doctor` to check the state \
         directory."
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod stop_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod agent_tests;
