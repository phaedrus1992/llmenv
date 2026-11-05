//! The one-line description of a checkpoint that doctor shows.

use std::path::Path;

use super::{Checkpoint, MAX_ATTEMPTS};

/// Doctor's description of one checkpoint: kind, age, attempts, phase, and what to read.
#[must_use]
pub(crate) fn describe(checkpoint: &Checkpoint, now: u64, log: &Path) -> String {
    let age = now.saturating_sub(checkpoint.started_at);
    format!(
        "{} started {} ago, {}/{MAX_ATTEMPTS} attempts, phase {}; log: {}",
        checkpoint.kind.as_str(),
        format_age(age),
        checkpoint.attempts,
        checkpoint.phase,
        log.display()
    )
}

pub(super) fn format_age(secs: u64) -> String {
    humantime::format_duration(std::time::Duration::from_secs(secs)).to_string()
}
