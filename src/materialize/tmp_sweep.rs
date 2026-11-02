//! Age sweep of the state temp folder, `state/tmp` (#2814).
//!
//! `TMPDIR` points at this folder, so every tool that honors it writes here. Nothing else
//! removes what those tools leave behind. An entry is removed when its newest mtime is older
//! than the retention, and no process holds a path inside it open.

mod claude_root;

mod remove;
mod summary;
pub use remove::sweep;
mod walk;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod walk_tests;
pub use summary::{stale_summary, stale_summary_in};

/// The stale entries of `state/tmp`: how many, and how many bytes they hold.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StaleSummary {
    pub count: usize,
    pub bytes: u64,
    /// True when `bytes` leaves out an entry that could not be measured, so it is a floor (#3069).
    pub bytes_partial: bool,
    /// Entries the scan could not read or judge. They are not in `count`.
    pub unreadable: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
