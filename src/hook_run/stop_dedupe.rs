//! Stop-hook loop guard (#2511).
//!
//! A Stop hook that returns text makes the engine start another turn, and that turn ends with
//! another Stop. Without a guard, an agent with nothing to do answers the same reminder until
//! the engine reaches its cap on consecutive hook blocks. Two rules end the loop:
//!
//! 1. A Stop payload with `stop_hook_active: true` follows a turn that a Stop hook caused. It
//!    gets no reminder.
//! 2. A reminder that equals the last one emitted for the session is not emitted again. Any
//!    change of the reminder text, an empty reminder, or a new user prompt re-arms it.
//!
//! The last reminder is kept as a hash in `state_dir/stop_dedupe/<session>.json`. A missing,
//! unreadable, or malformed file reads as "nothing emitted yet", so a damaged file can only
//! cause one extra reminder. A state dir that cannot be written, or a session id that is not a
//! safe file name, disables rule 2 and logs a warning. Rule 1 still applies. The task store
//! lives in the same dir, so the tracker is already broken in that case.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::session_ledger::hash_prefix;

const STALE_DAYS: u64 = 7;

#[derive(Debug, Serialize, Deserialize)]
struct LastReminder {
    hash: String,
}

/// Whether `payload` says a Stop hook caused this stop. A missing or non-boolean field is
/// `false`: an engine that does not send the field gets rule 2 only.
#[must_use]
pub(crate) fn is_hook_continuation(payload: &serde_json::Value) -> bool {
    payload
        .get("stop_hook_active")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// `reminder` when it is new for the session, else the empty string. Records `reminder` as the
/// last one emitted. An empty `reminder` clears the record. A call without a usable
/// `session_id` cannot be tracked and passes `reminder` through.
#[must_use]
pub(crate) fn emit_once(state_dir: &Path, session_id: Option<&str>, reminder: &str) -> String {
    let Some(path) = state_path(state_dir, session_id) else {
        return reminder.to_string();
    };
    if reminder.is_empty() {
        clear_last(&path);
        return String::new();
    }
    let hash = hash_prefix(reminder.as_bytes());
    if load_last(&path).is_some_and(|last| last.hash == hash) {
        return String::new();
    }
    save(&path, &LastReminder { hash });
    reminder.to_string()
}

/// Re-arm the guard for `session_id`: the next reminder is emitted even if it equals the last.
/// Called when the user sends a prompt, because a reminder about work that is still open is
/// due again on the next turn.
pub(crate) fn forget(state_dir: &Path, session_id: Option<&str>) {
    if let Some(path) = state_path(state_dir, session_id) {
        clear_last(&path);
    }
}

/// The record path, or `None` when `session_id` is absent or unsafe as a file name. The id
/// comes from the hook payload, so it is checked before it joins a path.
fn state_path(state_dir: &Path, session_id: Option<&str>) -> Option<PathBuf> {
    let id = session_id?;
    if !crate::paths::is_valid_short_name(id) {
        tracing::error!("session_id failed path-safety validation for stop_dedupe, ignoring");
        return None;
    }
    Some(state_dir.join("stop_dedupe").join(format!("{id}.json")))
}

fn load_last(path: &Path) -> Option<LastReminder> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("cannot read stop_dedupe state {}: {e}", path.display());
            return None;
        }
    };
    serde_json::from_str(&text)
        .map_err(|e| tracing::warn!("malformed stop_dedupe state {}: {e}", path.display()))
        .ok()
}

fn save(path: &Path, last: &LastReminder) {
    if let Some(dir) = path.parent() {
        super::session_state::prune_stale_json_files(dir, STALE_DAYS);
    }
    let result = serde_json::to_vec(last)
        .map_err(std::io::Error::other)
        .and_then(|json| crate::paths::write_owner_only_atomic(path, &json));
    if let Err(e) = result {
        tracing::warn!("cannot save stop_dedupe state {}: {e}", path.display());
    }
}

fn clear_last(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("cannot clear stop_dedupe state {}: {e}", path.display()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn hook_continuation_reads_only_a_true_boolean() {
        assert!(is_hook_continuation(&json!({ "stop_hook_active": true })));
        assert!(!is_hook_continuation(&json!({ "stop_hook_active": false })));
        assert!(!is_hook_continuation(
            &json!({ "stop_hook_active": "true" })
        ));
        assert!(!is_hook_continuation(&json!({})));
    }

    #[test]
    fn second_identical_reminder_is_silent() {
        let dir = TempDir::new().expect("test");
        assert_eq!(emit_once(dir.path(), Some("s1"), "do x"), "do x");
        assert_eq!(emit_once(dir.path(), Some("s1"), "do x"), "");
        assert_eq!(emit_once(dir.path(), Some("s1"), "do x"), "");
    }

    #[test]
    fn changed_reminder_is_emitted_and_becomes_the_new_baseline() {
        let dir = TempDir::new().expect("test");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
        assert_eq!(emit_once(dir.path(), Some("s1"), "b"), "b");
        assert_eq!(emit_once(dir.path(), Some("s1"), "b"), "");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
    }

    #[test]
    fn empty_reminder_re_arms_the_guard() {
        let dir = TempDir::new().expect("test");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
        assert_eq!(emit_once(dir.path(), Some("s1"), ""), "");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
    }

    #[test]
    fn forget_re_arms_the_guard() {
        let dir = TempDir::new().expect("test");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
        forget(dir.path(), Some("s1"));
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
    }

    #[test]
    fn sessions_do_not_share_a_baseline() {
        let dir = TempDir::new().expect("test");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
        assert_eq!(emit_once(dir.path(), Some("s2"), "a"), "a");
    }

    #[test]
    fn missing_or_unsafe_session_id_passes_through_every_time() {
        let dir = TempDir::new().expect("test");
        assert_eq!(emit_once(dir.path(), None, "a"), "a");
        assert_eq!(emit_once(dir.path(), None, "a"), "a");
        assert_eq!(emit_once(dir.path(), Some("../x"), "a"), "a");
        assert_eq!(emit_once(dir.path(), Some("../x"), "a"), "a");
        assert!(!dir.path().join("stop_dedupe").exists());
    }

    #[test]
    fn malformed_state_file_costs_one_extra_reminder() {
        let dir = TempDir::new().expect("test");
        let path = state_path(dir.path(), Some("s1")).expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "not json").expect("write");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "a");
        assert_eq!(emit_once(dir.path(), Some("s1"), "a"), "");
    }

    proptest! {
        /// A reminder that repeats with no change in between is emitted at most once, for any
        /// non-empty text and any repeat count.
        #[test]
        fn repeated_unchanged_reminder_is_emitted_once(
            text in "[ -~]{1,200}",
            repeats in 2usize..20,
        ) {
            let dir = TempDir::new().expect("test");
            let emitted = (0..repeats)
                .filter(|_| !emit_once(dir.path(), Some("s1"), &text).is_empty())
                .count();
            prop_assert_eq!(emitted, 1);
        }

        /// A reminder is emitted exactly when it differs from the one before it.
        #[test]
        fn emitted_exactly_when_the_text_changes(
            texts in proptest::collection::vec("[a-c]{1,2}", 1..30),
        ) {
            let dir = TempDir::new().expect("test");
            let mut previous: Option<&str> = None;
            for text in &texts {
                let out = emit_once(dir.path(), Some("s1"), text);
                prop_assert_eq!(!out.is_empty(), previous != Some(text.as_str()));
                previous = Some(text);
            }
        }
    }
}
