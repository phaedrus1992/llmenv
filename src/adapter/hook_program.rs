//! The command line that runs llmenv's built-in hooks, rendered into each engine's hook table
//! (#2956, #3081).

use std::path::PathBuf;

use crate::util::shell_word;

/// The command line that starts llmenv with `args`, such as `hook-run --engine crush`.
///
/// The program is the running llmenv binary, shell-quoted. `PATH` is not searched, so a
/// project folder on `PATH` cannot be written into an engine config. When the binary's path is
/// not UTF-8 or the OS cannot report it, the bare name is used and an error is logged.
pub(crate) fn hook_command(args: &str) -> String {
    hook_command_in(std::env::current_exe(), args)
}

/// Pick how hook commands name llmenv, from the running binary's path in `exe`.
///
/// `Ok` holds the shell-quoted path. `Err` holds the reason the path cannot be used.
pub(crate) fn program_name(exe: std::io::Result<PathBuf>) -> Result<String, String> {
    match exe {
        Ok(path) if path.to_str().is_some() => Ok(shell_word(&path)),
        // A lossy conversion would name a file that does not exist, so no path is used.
        Ok(path) => Err(format!(
            "llmenv is at {}, which is not valid UTF-8",
            path.display()
        )),
        Err(e) => Err(format!("the path of the running llmenv is unknown ({e})")),
    }
}

/// The message for the bare-name fallback. It names the reason and the fix.
pub(crate) fn fallback_message(reason: &str) -> String {
    format!(
        "{reason}, so hook commands use the bare name `llmenv`, which a PATH entry can shadow. \
         Fix: move the llmenv binary to a UTF-8 path, then regenerate the hook config."
    )
}

fn hook_command_in(exe: std::io::Result<PathBuf>, args: &str) -> String {
    let program = program_name(exe).unwrap_or_else(|reason| {
        tracing::error!("{}", fallback_message(&reason));
        "llmenv".to_string()
    });
    format!("{program} {args}")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn renders_the_running_binary_path_not_a_path_search() {
        let exe = PathBuf::from("/opt/llmenv/bin/llmenv");
        let command = hook_command_in(Ok(exe), "hook-run --engine crush pre_tool_use");
        assert_eq!(
            command,
            "/opt/llmenv/bin/llmenv hook-run --engine crush pre_tool_use"
        );
    }

    #[test]
    fn a_known_binary_path_is_used_without_a_fallback() {
        let program = program_name(Ok(PathBuf::from("/opt/llmenv/bin/llmenv")));
        assert_eq!(program, Ok("/opt/llmenv/bin/llmenv".to_string()));
    }

    #[test]
    fn an_unknown_binary_path_falls_back_to_the_bare_name_with_the_reason() {
        let reason = program_name(Err(std::io::Error::other("no such file")))
            .expect_err("an unknown path has a reason");
        assert!(reason.contains("no such file"), "{reason}");
        assert_eq!(
            hook_command_in(Err(std::io::Error::other("gone")), "throttle"),
            "llmenv throttle"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_binary_path_falls_back_to_the_bare_name_with_the_reason() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let exe = PathBuf::from(OsString::from_vec(vec![b'/', 0xff, b'/', b'l']));
        let reason = program_name(Ok(exe)).expect_err("a non-UTF-8 path has a reason");
        assert!(reason.contains("not valid UTF-8"), "{reason}");
    }

    #[test]
    fn the_fallback_message_names_the_fix() {
        let message = fallback_message("the path is unknown");
        assert!(message.contains("Fix: move the llmenv binary"), "{message}");
    }

    proptest! {
        /// The quoted program is one shell word: a shell that reads it back gets the same text.
        #[cfg(unix)]
        #[test]
        fn quoted_program_round_trips_through_a_shell(text in "[^\u{0}]{1,40}") {
            let word = shell_word(std::path::Path::new(&text));
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf '%s' {word}"))
                .output()
                .expect("run /bin/sh");
            prop_assert_eq!(String::from_utf8_lossy(&out.stdout).into_owned(), text);
        }
    }
}
