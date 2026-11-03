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
