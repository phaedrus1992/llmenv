//! Reads of the slippage stats log. A missing log is the first-use case. An unreadable or corrupt
//! log is a fault, and its reason names the log file and the fix.

use super::{SessionStats, stats_path};

/// Read the stats log of one session. A missing log reads as empty.
///
/// # Errors
/// Returns the reason when the log cannot be read or parsed. The reason names the log file.
pub(super) fn read_stats(
    state_dir: &std::path::Path,
    session_id: &str,
) -> Result<SessionStats, String> {
    let path = stats_path(state_dir, session_id);
    let text = match crate::paths::bounded::state_text_io(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SessionStats::default()),
        Err(e) => {
            return Err(format!(
                "cannot read the slippage stats log {} ({e}). Fix: correct the problem in the error, or remove the file.",
                path.display()
            ));
        }
    };
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "the slippage stats log {} is corrupt ({e}). Fix: remove the file, or Read any file to rewrite the log.",
            path.display()
        )
    })
}

/// Load the stats log of one session. A bad log is recorded as an agent notice and reads as empty.
pub(in crate::hook_run) fn load_stats(
    state_dir: &std::path::Path,
    session_id: &str,
) -> SessionStats {
    // An unreadable or corrupt log means "nothing known to have been read". The write guard then
    // denies a Write to an existing file until the next Read rewrites the log.
    read_stats(state_dir, session_id).unwrap_or_else(|problem| {
        crate::agent_notice::record_logged(format!("llmenv: {problem}"));
        SessionStats::default()
    })
}

/// Load the stats log before the caller rewrites it. A bad log is moved to `<log>.corrupt` first,
/// so its bytes survive the rewrite (#3349).
///
/// Returns `None` when the move fails. The caller must then skip the rewrite, or the bytes are lost.
pub(in crate::hook_run) fn load_stats_for_rewrite(
    state_dir: &std::path::Path,
    session_id: &str,
) -> Option<SessionStats> {
    let problem = match read_stats(state_dir, session_id) {
        Ok(stats) => return Some(stats),
        Err(problem) => problem,
    };
    let path = stats_path(state_dir, session_id);
    let mut kept_name = path.as_os_str().to_owned();
    kept_name.push(".corrupt");
    let kept = std::path::PathBuf::from(kept_name);
    match std::fs::rename(&path, &kept) {
        Ok(()) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: {problem} The bad log was moved to {} and a new log starts.",
                kept.display()
            ));
            Some(SessionStats::default())
        }
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: {problem} The bad log could not be moved to {} ({e}), so this call did not rewrite it. Fix: remove the bad log, or the entry at that path.",
                kept.display()
            ));
            None
        }
    }
}
