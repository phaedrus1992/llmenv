//! Reads of the slippage stats log. A missing log is the first-use case. A corrupt log is moved
//! aside before a rewrite. Any other read failure keeps the file and skips the rewrite.

use std::path::{Path, PathBuf};

use super::{SessionStats, stats_path};

/// How many numbered copies a corrupt log may take before the move gives up.
const MOVE_ASIDE_TRIES: u32 = 100;

/// Why a stats log could not be read.
enum Fault {
    /// The bytes are not a usable stats log. A rewrite may move them aside.
    Corrupt(String),
    /// The file could not be read, for example because of a permission or an I/O error. The
    /// bytes may be fine, so a rewrite must not touch them.
    Unreadable(String),
}

/// Read the log at `path`, keeping a corrupt file apart from an unreadable one. A missing log
/// reads as empty.
fn read_checked(path: &Path) -> Result<SessionStats, Fault> {
    let text = match crate::paths::bounded::state_text_io(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SessionStats::default()),
        // Invalid UTF-8, an oversized file, and a folder all arrive as InvalidData. A regular
        // file is a log that this layer cannot use, so its bytes may move aside. Anything else
        // is left in place.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData && path.is_file() => {
            return Err(Fault::Corrupt(format!(
                "the slippage stats log {} is not a usable log ({e})",
                path.display()
            )));
        }
        Err(e) => {
            return Err(Fault::Unreadable(format!(
                "cannot read the slippage stats log {} ({e})",
                path.display()
            )));
        }
    };
    serde_json::from_str(&text).map_err(|e| {
        Fault::Corrupt(format!(
            "the slippage stats log {} is corrupt ({e})",
            path.display()
        ))
    })
}

/// Read the stats log of one session. A missing log reads as empty.
///
/// # Errors
/// Returns the reason when the log cannot be read or parsed. The reason names the log file.
pub(super) fn read_stats(state_dir: &Path, session_id: &str) -> Result<SessionStats, String> {
    read_checked(&stats_path(state_dir, session_id)).map_err(|fault| match fault {
        Fault::Corrupt(cause) => {
            format!("{cause}. Fix: remove the file, or Read any file to rewrite the log.")
        }
        Fault::Unreadable(cause) => {
            format!("{cause}. Fix: correct the problem in the error, or remove the file.")
        }
    })
}

/// Load the stats log of one session. A bad log is recorded as an agent notice and reads as empty.
pub(in crate::hook_run) fn load_stats(state_dir: &Path, session_id: &str) -> SessionStats {
    // An unreadable or corrupt log means "nothing known to have been read". The write guard then
    // denies a Write to an existing file until the next Read rewrites the log.
    read_stats(state_dir, session_id).unwrap_or_else(|problem| {
        crate::agent_notice::record_logged(format!("llmenv: {problem}"));
        SessionStats::default()
    })
}

/// Load the stats log before the caller rewrites it.
///
/// A corrupt log moves to `<log>.corrupt`, or to the next free numbered copy, so its bytes survive
/// the rewrite (#3349). An unreadable log is never moved. `locked` says whether the caller holds
/// the session's stats lock. Without the lock, another hook can write a new log between the read
/// and the rename, so the move is refused.
///
/// Returns `None` when the rewrite must be skipped.
pub(in crate::hook_run) fn load_stats_for_rewrite(
    state_dir: &Path,
    session_id: &str,
    locked: bool,
) -> Option<SessionStats> {
    let path = stats_path(state_dir, session_id);
    let cause = match read_checked(&path) {
        Ok(stats) => return Some(stats),
        Err(Fault::Unreadable(cause)) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: {cause}. This call did not rewrite the log. Fix: correct the cause in the error, then Read the file again."
            ));
            return None;
        }
        Err(Fault::Corrupt(cause)) => cause,
    };
    if !locked {
        crate::agent_notice::record_logged(format!(
            "llmenv: {cause}. The log is not locked, so it was not moved aside and this call did not rewrite it. Fix: correct the lock failure noticed above, then retry the call."
        ));
        return None;
    }
    match move_aside(&path) {
        Ok(kept) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: {cause}. The bad bytes are kept at {}, and a new log starts. llmenv removes the copy after 7 days. Fix: copy {} elsewhere if you need it longer.",
                kept.display(),
                kept.display()
            ));
            Some(SessionStats::default())
        }
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: {cause}. The bad bytes could not be moved aside ({e}), so this call did not rewrite the log. Fix: correct the folder permissions, then Read the file again."
            ));
            None
        }
    }
}

/// Rename `path` to the first free name among `<path>.corrupt`, `<path>.corrupt.1`, and so on. An
/// earlier copy is never replaced, so repeated corruption keeps every bad log (#3349).
fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let mut base = path.as_os_str().to_owned();
    base.push(".corrupt");
    let base = PathBuf::from(base);
    for n in 0..MOVE_ASIDE_TRIES {
        let candidate = if n == 0 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{n}", base.display()))
        };
        if candidate.exists() {
            continue;
        }
        std::fs::rename(path, &candidate)?;
        return Ok(candidate);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "no free copy name after {MOVE_ASIDE_TRIES} tries next to {}",
            base.display()
        ),
    ))
}
