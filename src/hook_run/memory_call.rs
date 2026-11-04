//! What a memory call knows about the hook event that makes it (#3218).

/// What a memory call needs to know about the hook event that makes it.
pub(super) struct MemoryCallContext<'a, E> {
    /// The hook event, for notice text.
    pub(super) event: E,
    /// The llmenv state folder, if known.
    pub(super) state_dir: Option<&'a std::path::Path>,
    /// The engine conversation id from the hook input, if any.
    pub(super) session_id: Option<&'a str>,
}

/// The llmenv state folder for a notice that is kept once per session. A failure is noticed,
/// because without the folder the notice repeats on every event.
pub(super) fn notice_state_dir() -> Option<std::path::PathBuf> {
    crate::paths::state_dir()
        .inspect_err(|e| {
            crate::agent_notice::record_logged(format!(
                "llmenv: the state folder cannot be found, so a bundle notice repeats on every \
                 event: {e:#}. Fix: set HOME."
            ));
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_state_dir_is_the_state_folder() {
        assert_eq!(notice_state_dir(), crate::paths::state_dir().ok());
    }
}
