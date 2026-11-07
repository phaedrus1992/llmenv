//! Persistence of the per-session nudge state: one JSON file per session, written atomically.

use std::path::{Path, PathBuf};

use super::NudgeState;

pub(super) fn state_path(state_dir: &Path, session_id: &str) -> Option<PathBuf> {
    // A session id becomes a file name, so only a plain name is accepted.
    crate::paths::is_valid_short_name(session_id).then(|| {
        state_dir
            .join("task_nudge")
            .join(format!("{session_id}.json"))
    })
}

/// The state of a session, and whether it was reset. A missing file is a new session. A corrupt
/// file is reset, and the caller saves the reset so the next call does not report it again. A
/// file that exists but cannot be read is an error, and the message names the path.
pub(super) fn read_state(path: &Path) -> Result<(NudgeState, bool), String> {
    let text = match crate::paths::bounded::state_json_text_io(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((NudgeState::default(), false));
        }
        Err(e) => return Err(format!("{} cannot be read: {e}", path.display())),
    };
    Ok(match serde_json::from_str(&text) {
        Ok(state) => (state, false),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv found an unreadable task nudge state {}, so its nudge history was reset: \
                 {e}. Fix: none needed; the reset state replaces the file.",
                path.display()
            ));
            (NudgeState::default(), true)
        }
    })
}

/// [`read_state`] for the hooks that fail soft: the error is logged and reaches the agent, and the
/// caller skips its nudge.
pub(super) fn load(path: &Path) -> Option<(NudgeState, bool)> {
    read_state(path)
        .inspect_err(|e| {
            tracing::error!("task nudge state {e}");
            crate::agent_notice::record(format!(
                "llmenv skipped the task nudge check: task nudge state {e}. Fix: check the file's \
                 permissions."
            ));
        })
        .ok()
}

/// Whether the state was saved. A caller that depends on the state, such as the deny marker,
/// must not act when it was not.
pub(super) fn save(path: &Path, state: &NudgeState) -> bool {
    let written = path
        .parent()
        .map_or(Ok(()), crate::paths::create_dir_owner_only)
        .and_then(|()| {
            let json = serde_json::to_string(state)?;
            crate::paths::write_owner_only_atomic(path, json.as_bytes())
        });
    if let Err(e) = &written {
        // A lost save repeats the same nudge on every call, so the agent is told (#2536).
        crate::agent_notice::record_logged(format!(
            "llmenv: the task nudge state {} cannot be saved: {e:#}. A nudge may repeat. Fix: \
             check the permissions of the state directory.",
            path.display()
        ));
    }
    written.is_ok()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    /// A state file cut inside its last character is reset like a corrupt file, not refused as
    /// unreadable, so the commit gate does not deny on it (#3457).
    #[test]
    fn a_state_file_cut_inside_its_last_character_is_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, b"{\"note\": \"caf\xc3").unwrap();
        let (state, reset) = read_state(&path).unwrap();
        assert!(reset, "the cut file is reset");
        assert_eq!(state, NudgeState::default());
    }

    fn nudge_state() -> impl proptest::strategy::Strategy<Value = NudgeState> {
        use proptest::prelude::*;
        let texts = || proptest::collection::vec("[a-zé€😀 -]{0,6}", 0..3);
        (
            (any::<u32>(), any::<u32>(), any::<bool>(), any::<bool>()),
            proptest::option::of("[a-zé€😀-]{0,8}"),
            texts(),
            any::<u32>(),
            texts(),
        )
            .prop_map(
                |(
                    (calls, last_nudge, skill_reminded, commit_denied),
                    start,
                    boundary,
                    quiet,
                    asked,
                )| {
                    NudgeState {
                        calls,
                        last_nudge,
                        skill_reminded,
                        commit_denied,
                        start_reminded: start,
                        boundary_reminded: boundary,
                        stop_quiet_turns: quiet,
                        ownerless_question_reminded: asked,
                    }
                },
            )
    }

    proptest::proptest! {
        /// A saved state reads back unchanged, with no reset.
        #[test]
        fn a_saved_state_reads_back_unchanged(state in nudge_state()) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("task_nudge").join("s.json");
            proptest::prop_assert!(save(&path, &state));
            proptest::prop_assert_eq!(read_state(&path).unwrap(), (state, false));
        }

        /// Any cut of a saved state, inside a character or not, resets to the default state and
        /// never reads as a partial state (#3457).
        #[test]
        fn any_cut_of_a_saved_state_resets(
            state in nudge_state(),
            cut in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.json");
            let saved = serde_json::to_vec(&state).unwrap();
            std::fs::write(&path, &saved[..cut.index(saved.len())]).unwrap();
            proptest::prop_assert_eq!(read_state(&path).unwrap(), (NudgeState::default(), true));
        }
    }
}
