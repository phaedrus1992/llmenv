//! The error of a session-log prune pass that failed after it removed some files.

use std::path::PathBuf;

/// A prune pass that failed. It keeps the files it removed before the failure, so a caller can
/// report them next to the error.
#[derive(Debug)]
pub(crate) struct PruneError {
    /// The files that the pass removed, or would remove in a dry run.
    pub(crate) removed: Vec<PathBuf>,
    message: String,
}

impl PruneError {
    /// An error before any file was judged, so no file was removed.
    pub(crate) fn without_removals(message: String) -> Self {
        Self {
            removed: Vec::new(),
            message,
        }
    }

    /// An error that follows some removals. The message names the failures.
    pub(crate) fn with_removals(removed: Vec<PathBuf>, message: String) -> Self {
        Self { removed, message }
    }
}

impl std::fmt::Display for PruneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PruneError {}
