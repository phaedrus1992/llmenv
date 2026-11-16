//! The error of a bulk task write that failed after it had changed some tasks (#3749).

use super::Task;

/// A save failed part way through a list of tasks. `changed` holds the tasks that were saved
/// before the failure, so the caller can record its audit note and touch their sessions before
/// it reports the error. A caller that uses `?` still gets the failure text.
#[derive(Debug)]
pub(crate) struct PartialWrite {
    changed: Vec<Task>,
    source: anyhow::Error,
}

impl std::fmt::Display for PartialWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

impl std::error::Error for PartialWrite {}

impl PartialWrite {
    /// Wrap `source` as an [`anyhow::Error`] that carries the tasks that were already saved.
    pub(crate) fn error(changed: Vec<Task>, source: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self { changed, source })
    }
}

/// The tasks that `error` says were saved before it failed. Empty for any other error.
pub(crate) fn changed_before_failure(error: &anyhow::Error) -> &[Task] {
    error
        .downcast_ref::<PartialWrite>()
        .map_or(&[], |p| p.changed.as_slice())
}
