//! Checkpoint reads for the tests of the job spawners.

use std::path::Path;

use super::{Entry, JobKind, list};

/// The kind, inputs, and session of each readable checkpoint, for the tests of the spawners.
pub(crate) fn checkpoint_entries_for_test(
    state_dir: &Path,
) -> Vec<(JobKind, serde_json::Value, Option<String>)> {
    list(state_dir)
        .into_iter()
        .filter_map(|e| match e {
            Entry::Ready(_, c) => Some((c.kind, c.inputs, c.session_id)),
            Entry::Unreadable(..) => None,
        })
        .collect()
}
