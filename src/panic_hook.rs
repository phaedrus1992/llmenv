//! Panic hook for the `llmenv` binary (#2554).
//!
//! The release profile sets `panic = "abort"`, so a panic ends the process in `abort()`. A
//! `println!` into a closed pipe panics, so `llmenv task ls | head` ended in `SIGABRT` and a
//! crash report. This hook exits quietly for that one case. Every other panic is logged, then
//! handed to the default hook, which prints it and aborts as before.

use std::panic::PanicHookInfo;

/// Exit status for a process that ends on a closed stdout: 128 plus SIGPIPE (13), the status a
/// shell reports for a process that a closed pipe ended.
const CLOSED_PIPE_STATUS: i32 = 141;

/// Install the hook. Call once, first thing in `main`, before any output.
pub fn install() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = payload_text(info);
        if is_closed_stdout(&message) {
            exit_on_closed_pipe();
        }
        tracing::error!(location = %location(info), "panic: {message}");
        default(info);
    }));
}

/// The panic message. A `panic!` with format arguments carries a `String`. A literal carries a
/// `&str`. Any other payload has no text to log.
fn payload_text(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_else(|| "(non-text panic payload)".to_string())
}

/// `file:line` of the panic, or `unknown` when the runtime does not record one.
fn location(info: &PanicHookInfo<'_>) -> String {
    info.location().map_or_else(
        || "unknown".to_string(),
        |l| format!("{}:{}", l.file(), l.line()),
    )
}

/// Whether a panic message is a failed write to stdout caused by a closed pipe.
///
/// The standard library builds this text as `failed printing to stdout: <io error>`. Only the
/// `Broken pipe` error means the reader left. A full disk or another error still aborts, so it
/// stays visible. The payload is text only, so matching on the message is the only option.
fn is_closed_stdout(message: &str) -> bool {
    message.starts_with("failed printing to stdout") && message.contains("Broken pipe")
}

/// End the process without `abort()`. The workspace denies `process::exit`; this is the one
/// place a panic hook may end the process, and only for a closed pipe.
#[expect(
    clippy::exit,
    reason = "panic hook: end on a closed stdout without SIGABRT (#2554)"
)]
fn exit_on_closed_pipe() -> ! {
    std::process::exit(CLOSED_PIPE_STATUS)
}

#[cfg(test)]
mod tests {
    use super::is_closed_stdout;

    #[test]
    fn the_std_closed_pipe_message_is_recognised() {
        assert!(is_closed_stdout(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn other_stdout_failures_still_abort_loudly() {
        assert!(!is_closed_stdout(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
    }

    #[test]
    fn a_closed_stderr_is_not_treated_as_a_closed_stdout() {
        assert!(!is_closed_stdout(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn an_unrelated_panic_is_not_treated_as_a_closed_stdout() {
        assert!(!is_closed_stdout("index out of bounds: the len is 0"));
        assert!(!is_closed_stdout(""));
    }

    #[test]
    fn a_message_that_only_mentions_the_text_is_not_matched() {
        assert!(!is_closed_stdout(
            "config mentions failed printing to stdout and Broken pipe"
        ));
    }
}
