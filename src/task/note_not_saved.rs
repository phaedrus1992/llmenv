//! The error of a command whose change is saved but whose audit note is not (#3765).
//!
//! The exit status of such a command is [`EXIT_SAVED_WITHOUT_NOTE`]. A script, or an agent that
//! retries a failed command, can then tell it from a command whose change failed, which exits with
//! status 1, and it does not repeat the change.

/// The exit status of a command that saved its change and failed to write the audit note.
pub const EXIT_SAVED_WITHOUT_NOTE: i32 = 3;

/// The failed note of a saved change. The text is the full text of the failure, so the error reads
/// as before.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct NoteNotSaved(String);

/// Mark `error` as the failure of an audit note after a saved change. The text does not change.
pub(crate) fn mark(error: &anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(NoteNotSaved(format!("{error:#}")))
}

/// [`mark`] for the error of a result.
pub(crate) fn mark_result<T>(result: anyhow::Result<T>) -> anyhow::Result<T> {
    result.map_err(|e| mark(&e))
}

/// The exit status for `error`: [`EXIT_SAVED_WITHOUT_NOTE`] when it holds a failed note after a
/// saved change, and 1 for any other failure.
#[must_use]
pub fn exit_code(error: &anyhow::Error) -> i32 {
    if error.downcast_ref::<NoteNotSaved>().is_some() {
        EXIT_SAVED_WITHOUT_NOTE
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marked_error_reads_the_same_and_exits_with_three() {
        let plain = anyhow::anyhow!("inner").context("outer");
        let marked = mark(&plain);
        assert_eq!(format!("{marked:#}"), format!("{plain:#}"));
        assert_eq!(exit_code(&marked), EXIT_SAVED_WITHOUT_NOTE);
        assert_eq!(exit_code(&plain), 1);
    }

    #[test]
    fn a_context_around_a_marked_error_keeps_the_status() {
        let wrapped = mark(&anyhow::anyhow!("note")).context("also failed");
        assert_eq!(exit_code(&wrapped), EXIT_SAVED_WITHOUT_NOTE);
    }
}
