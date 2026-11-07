//! The `--checkpoint` argument of a detached child, and the spool that carries its notices.

use std::path::PathBuf;

use crate::paths;

/// The `--checkpoint` path of a detached child, checked to be a checkpoint file under the state
/// dir (#2396). The child deletes and trusts the file, so it must not take an arbitrary path.
/// Every detached child calls this first, so it also spools the child's notices for the next
/// hook run (#3398).
pub(super) fn checked_checkpoint(path: Option<PathBuf>) -> anyhow::Result<Option<PathBuf>> {
    match paths::state_dir() {
        Ok(state_dir) => crate::agent_notice::start_spool(&state_dir),
        // Without the spool, a detached job cannot report its notices, so the loss is named (#3507).
        Err(e) => crate::agent_notice::no_state_folder("detached job notices are not recorded", &e),
    }
    let Some(path) = path else {
        return Ok(None);
    };
    let state_dir = paths::state_dir()?;
    crate::hook_run::checkpoint::validated_path(&state_dir, &path).map(Some)
}
