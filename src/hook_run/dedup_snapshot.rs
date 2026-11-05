//! The SessionEnd dedup snapshot: where it lives, and whether a store writes it.

use super::HookEvent;

/// The SessionEnd dedup snapshot path. Other events have none. A missing state directory skips
/// the snapshot and logs, because an error return would drop the notices built above (#2719).
pub(super) fn dedup_snapshot_path(
    event: HookEvent,
    state_dir: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if event != HookEvent::SessionEnd {
        return None;
    }
    match state_dir() {
        Ok(dir) => Some(dir.join(crate::paths::HOOK_STORE_CHUNK)),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the session-end duplicate check is skipped, because the state folder \
                 cannot be found, so the context chunk is stored again: {e}. Fix: set HOME, or \
                 set LLMENV_STATE_DIR to a valid absolute path."
            ));
            None
        }
    }
}

/// The dedup snapshot to write after a store. A chunk the store already matched is not written
/// again, so the snapshot keeps its earlier content.
pub(super) fn dedup_write_target(
    path: Option<&std::path::Path>,
    unchanged: bool,
) -> Option<&std::path::Path> {
    path.filter(|_| !unchanged)
}
