//! The error of a recall call that a time budget cut off.

/// Returned for a call that the budget cut off. The breaker counts it as a failed call, because
/// a server that is always slower than the budget never answers in time.
#[derive(Debug, thiserror::Error)]
#[error("the {flow} recall budget of {secs} s ran out")]
pub(crate) struct Cut {
    flow: &'static str,
    secs: u64,
}

impl Cut {
    pub(super) fn new(flow: &'static str, secs: u64) -> Self {
        Self { flow, secs }
    }
}
