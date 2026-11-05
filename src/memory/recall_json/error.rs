//! The errors of the JSON recall reader (#3368).

/// Why a recall reply is not a list of records.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum RecallJsonError {
    /// The reply does not start with a JSON array. icm before 0.11.5 can answer in text.
    #[error(
        "the icm_memory_recall reply is not a JSON list ({reason}); it starts with {start:?}. \
         Fix: upgrade icm to 0.11.5 or later"
    )]
    NotAList { reason: String, start: String },
    /// One record of the list does not have the fields of an icm record.
    #[error(
        "record {index} of the icm_memory_recall reply is not an icm record ({reason}): {record}. \
         Fix: upgrade icm to 0.11.5 or later, or report the reply shape"
    )]
    BadRecord {
        index: usize,
        reason: String,
        record: String,
    },
}
