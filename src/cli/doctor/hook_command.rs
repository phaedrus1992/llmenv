//! Doctor check for the program name in llmenv's hook commands (#3049, #3081). The hook commands
//! name the running llmenv binary. The bare name is used only when that binary's path is not
//! UTF-8 or the OS cannot report it.

use crate::adapter::hook_program::{fallback_message, program_name};

/// Print the hook-command check for the running binary.
pub(super) fn run_doctor_hook_command(use_color: bool) {
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    match program_name(std::env::current_exe()) {
        Ok(_) => eprintln!("{pass} Hook commands name llmenv by the path of the running binary"),
        Err(reason) => eprintln!("{warn} {}", fallback_message(&reason)),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_binary_path_is_reported_with_the_fix() {
        let reason = program_name(Err(std::io::Error::other("no path")))
            .expect_err("an unknown path is reported");
        let message = fallback_message(&reason);
        assert!(message.contains("Fix: move the llmenv binary"), "{message}");
    }

    #[test]
    fn a_known_binary_path_is_not_reported() {
        let exe = std::path::PathBuf::from("/opt/llmenv/bin/llmenv");
        assert!(program_name(Ok(exe)).is_ok());
    }
}
