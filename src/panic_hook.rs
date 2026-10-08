//! Panic hook for the `llmenv` binary (#2554, #2561).
//!
//! The release profile sets `panic = "abort"`, so a panic ends the process in `abort()`. A
//! `println!` into a closed pipe panics, so `llmenv task ls | head` ended in `SIGABRT` and a
//! crash report. This hook exits quietly for a closed stdout or a closed stderr. Every other
//! panic is logged, then handed to the default hook, which prints it and aborts as before.

/// Exit status for a process that ends on a closed pipe: 128 plus SIGPIPE (13), the status a
/// shell reports for a process that a closed pipe ended.
const CLOSED_PIPE_STATUS: i32 = 141;

/// Install the hook. Call once, first thing in `main`, before any output.
pub fn install() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `payload_as_str` covers a `String` and a `&str` payload. Any other payload has no text.
        let message = info.payload_as_str().unwrap_or("(non-text panic payload)");
        if is_closed_pipe(message) {
            // The write that failed is the reader's loss, not a crash. The log keeps the record,
            // because stderr may be the closed stream and the reader will not see this line.
            tracing::warn!("exiting quietly after a closed pipe: {message}");
            exit_on_closed_pipe();
        }
        let location = info.location().map_or_else(
            || "unknown".to_string(),
            |l| format!("{}:{}", l.file(), l.line()),
        );
        tracing::error!(location = %location, "panic: {message}");
        default(info);
    }));
}

/// Whether a panic message is a failed write to stdout or stderr caused by a closed pipe.
///
/// The standard library builds this text as `failed printing to <stream>: <io error>`. Only the
/// `Broken pipe` error means the reader left. A full disk or another error still aborts, so it
/// stays visible. The payload is text only, so matching on the message is the only option.
fn is_closed_pipe(message: &str) -> bool {
    let from_stream = message.starts_with("failed printing to stdout")
        || message.starts_with("failed printing to stderr");
    from_stream && message.contains("Broken pipe")
}

/// End the process without `abort()`. The workspace denies `process::exit`; this is the one
/// place a panic hook may end the process, and only for a closed pipe.
#[expect(
    clippy::exit,
    reason = "panic hook: end on a closed pipe without SIGABRT (#2554, #2561)"
)]
fn exit_on_closed_pipe() -> ! {
    std::process::exit(CLOSED_PIPE_STATUS)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::is_closed_pipe;

    proptest! {
        #[test]
        fn the_stdout_prefix_with_any_error_matches_only_broken_pipe(suffix in ".{0,40}") {
            let message = format!("failed printing to stdout: {suffix}");
            prop_assert_eq!(is_closed_pipe(&message), suffix.contains("Broken pipe"));
        }

        #[test]
        fn the_stderr_prefix_with_any_error_matches_only_broken_pipe(suffix in ".{0,40}") {
            let message = format!("failed printing to stderr: {suffix}");
            prop_assert_eq!(is_closed_pipe(&message), suffix.contains("Broken pipe"));
        }

        #[test]
        fn a_message_without_a_print_prefix_never_matches(rest in ".{0,60}") {
            prop_assume!(!rest.starts_with("failed printing to "));
            prop_assert!(!is_closed_pipe(&rest));
        }
    }

    #[test]
    fn the_std_closed_stdout_message_is_recognised() {
        assert!(is_closed_pipe(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn the_std_closed_stderr_message_is_recognised() {
        assert!(is_closed_pipe(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn other_stdout_failures_still_abort_loudly() {
        assert!(!is_closed_pipe(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
    }

    #[test]
    fn an_unrelated_panic_is_not_treated_as_a_closed_pipe() {
        assert!(!is_closed_pipe("index out of bounds: the len is 0"));
        assert!(!is_closed_pipe(""));
    }

    #[test]
    fn a_message_that_only_mentions_the_text_is_not_matched() {
        assert!(!is_closed_pipe(
            "config mentions failed printing to stdout and Broken pipe"
        ));
    }
}
