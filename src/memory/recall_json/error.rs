//! The errors of the JSON recall reader (#3368).

/// Why a recall reply is not a list of records.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum RecallJsonError {
    /// The reply has no text. icm answers `[]` when nothing matches.
    #[error(
        "the icm_memory_recall reply is empty, but icm answers [] when nothing matches. Fix: run \
         `llmenv doctor` to check the memory server and the proxy in front of it"
    )]
    Empty,
    /// The list stops before its end, such as at the client's body size limit.
    #[error(
        "the icm_memory_recall reply stops inside the record list after {bytes} bytes ({reason}). \
         Fix: run `llmenv doctor` to check the memory server, or lower the recall limit"
    )]
    CutOff { bytes: usize, reason: String },
    /// The reply does not start with a JSON array. icm before 0.11.5 answers in text.
    #[error(
        "the icm_memory_recall reply is not a JSON list ({reason}). \
         Fix: upgrade icm to 0.11.5 or later"
    )]
    NotAList { reason: String },
    /// One element of the list does not have the fields of an icm record.
    #[error(
        "record {index} of the icm_memory_recall reply is not an icm record ({reason}); it is \
         {shape}. Fix: upgrade icm to 0.11.5 or later, or report the reply shape to llmenv"
    )]
    BadRecord {
        index: usize,
        reason: String,
        shape: String,
    },
}
