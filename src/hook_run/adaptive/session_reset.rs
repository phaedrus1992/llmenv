//! The session-start checks for a context reset and for a ledger update that did not save
//! (#2766, #2767, #2863).

use super::{RECALL_STATE_UNSAVED, RESET_UNDONE};

/// Whether this `SessionStart` source starts a new context, so the ledger forgets what it sent.
pub(super) fn resets_ledger(source: Option<&str>) -> bool {
    super::super::session_state::context_was_lost(source)
}

/// What became of the first session-start ledger update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FirstUpdate {
    /// The ledger was read, changed, and saved.
    Saved,
    /// The lock was busy, or the file could not be read. Nothing was changed.
    NotApplied,
    /// The change was made in memory, but the save failed.
    SaveFailed,
}

impl FirstUpdate {
    /// Maps the result of `LedgerStore::update_reporting_save`.
    pub(super) fn of<T>(updated: Option<&(T, bool)>) -> Self {
        match updated {
            None => Self::NotApplied,
            Some((_, true)) => Self::Saved,
            Some((_, false)) => Self::SaveFailed,
        }
    }
}

/// The notice for the first session-start ledger update, or `None` when no notice is due.
/// The first update is the only write that carries this session's reset and project, so a
/// failure here is noticed even when the second update later succeeds (#2767).
///
/// A failed save is already noticed by the ledger. A requested reset adds `RESET_UNDONE`,
/// because the agent must hear that the reset did not reach disk (#2863).
pub(super) fn first_update_notice(update: FirstUpdate, reset: bool) -> Option<&'static str> {
    match (update, reset) {
        (FirstUpdate::Saved, _) => None,
        (FirstUpdate::SaveFailed, false) => None,
        (FirstUpdate::SaveFailed | FirstUpdate::NotApplied, true) => Some(RESET_UNDONE),
        (FirstUpdate::NotApplied, false) => Some(RECALL_STATE_UNSAVED),
    }
}
