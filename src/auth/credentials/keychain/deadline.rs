//! Deadlines for the blocking keychain calls, so a locked keychain cannot hang llmenv.

use std::time::Duration;

/// The message for a keychain call that gave no answer within `timeout`.
#[cfg(any(target_os = "macos", test))]
pub(super) fn timeout_error(operation: &str, service: &str, timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "`security {operation}` for keychain item {service} gave no answer within {} s, so the outcome is unknown. Fix: \
         unlock the login keychain (`security unlock-keychain`), answer any macOS keychain \
         prompt, then run the command again.",
        timeout.as_secs()
    )
}

/// Why a call run by [`with_deadline`] gave no answer.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoAnswer {
    /// The call was still running at the deadline.
    TimedOut,
    /// The worker ended before it sent an answer, for example after a panic (#3067 review).
    WorkerLost,
}

/// Run `call` on a worker thread for at most `timeout`. A call past the deadline keeps its thread,
/// because the Security framework gives no way to stop it, and the caller gets [`NoAnswer`].
#[cfg(any(target_os = "macos", test))]
pub(super) fn with_deadline<T: Send + 'static>(
    timeout: Duration,
    call: impl FnOnce() -> T + Send + 'static,
) -> Result<T, NoAnswer> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The receiver is gone after a timeout, so a late answer has no reader.
        let _ = tx.send(call());
    });
    rx.recv_timeout(timeout).map_err(|e| match e {
        std::sync::mpsc::RecvTimeoutError::Timeout => NoAnswer::TimedOut,
        std::sync::mpsc::RecvTimeoutError::Disconnected => NoAnswer::WorkerLost,
    })
}
