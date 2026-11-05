//! The command line that runs llmenv's built-in hooks, rendered into each engine's hook table
//! (#2956, #3081).

use std::path::PathBuf;

use crate::util::shell_word;

/// The command line that starts llmenv with `args`, such as `hook-run --engine crush pre_tool_use`.
///
/// # Errors
/// The running binary's path cannot name llmenv in a hook command. See [`llmenv_program`].
pub(crate) fn hook_command(args: &str) -> anyhow::Result<String> {
    hook_command_in(std::env::current_exe(), args)
}

/// The shell-quoted path of the running llmenv binary, which every hook command names.
///
/// `PATH` is not searched, so a project folder on `PATH` cannot be written into an engine config.
/// `llmenv doctor` and the hook commands both use this lookup, so they cannot disagree (#3060).
///
/// # Errors
/// The OS cannot report the binary's path, or the path is not UTF-8. A bare `llmenv` would let a
/// `PATH` entry shadow llmenv, so no fallback is used and the export fails (#3060).
pub(crate) fn llmenv_program() -> anyhow::Result<String> {
    program_in(std::env::current_exe())
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

/// The error for a binary path that hook commands cannot name. It names the reason and the fix.
fn unusable_program(reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{reason}, so llmenv cannot write its hook commands: a bare `llmenv` would let a PATH \
         entry run another program. Fix: install llmenv at a path that is valid UTF-8, then run \
         `llmenv regenerate`"
    )
}

fn program_in(exe: std::io::Result<PathBuf>) -> anyhow::Result<String> {
    program_name(exe).map_err(|reason| unusable_program(&reason))
}

fn hook_command_in(exe: std::io::Result<PathBuf>, args: &str) -> anyhow::Result<String> {
    Ok(format!("{} {args}", program_in(exe)?))
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
        let command = hook_command_in(Ok(exe), "hook-run --engine crush pre_tool_use")
            .expect("a UTF-8 path is used");
        assert_eq!(
            command,
            "/opt/llmenv/bin/llmenv hook-run --engine crush pre_tool_use"
        );
    }

    #[test]
    fn a_known_binary_path_is_used() {
        let program = program_name(Ok(PathBuf::from("/opt/llmenv/bin/llmenv")));
        assert_eq!(program, Ok("/opt/llmenv/bin/llmenv".to_string()));
    }

    #[test]
    fn an_unknown_binary_path_is_an_error_that_names_the_cause_and_the_fix() {
        let err = hook_command_in(Err(std::io::Error::other("no such file")), "throttle")
            .expect_err("an unknown path writes no hook command");
        let text = err.to_string();
        assert!(text.contains("no such file"), "{text}");
        assert!(text.contains("Fix: install llmenv at a path"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_binary_path_is_an_error_that_names_the_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let exe = PathBuf::from(OsString::from_vec(vec![b'/', 0xff, b'/', b'l']));
        let err = hook_command_in(Ok(exe), "throttle").expect_err("a non-UTF-8 path is refused");
        let text = err.to_string();
        assert!(text.contains("not valid UTF-8"), "{text}");
        assert!(text.contains("llmenv regenerate"), "{text}");
    }

    #[test]
    fn the_doctor_lookup_and_the_hook_command_agree() {
        let program = llmenv_program().expect("the test binary has a UTF-8 path");
        let command = hook_command("throttle").expect("the test binary has a UTF-8 path");
        assert_eq!(command, format!("{program} throttle"));
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
