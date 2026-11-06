//! Classifies a failed `git symbolic-ref` run for the task tracker (#3040).

/// Whether a failed `git symbolic-ref --quiet --short HEAD` is worth telling the user about.
/// A silent exit 1 is a detached HEAD (`--quiet` prints nothing for it), and exit 128 with "not
/// a git repository" is a directory outside git. Both are normal. Anything else, such as
/// "dubious ownership", a kill by signal, an exit 1 with an error text, or the phrase from
/// another exit code, hides the branch for a reason the user can fix (#3433).
pub(super) fn branch_failure_is_unexpected(code: Option<i32>, stderr: &str) -> bool {
    let detached = code == Some(1) && stderr.trim().is_empty();
    !detached && !crate::git::is_outside_repo(code, stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_failure_is_unexpected_ignores_a_detached_head_and_a_directory_outside_git() {
        assert!(!branch_failure_is_unexpected(Some(1), ""));
        assert!(!branch_failure_is_unexpected(
            Some(128),
            "fatal: not a git repository (or any of the parent directories): .git"
        ));
    }

    #[test]
    fn branch_failure_is_unexpected_ignores_a_capitalised_not_a_git_repository() {
        // git may capitalise its message, so the match ignores case (#3040).
        assert!(!branch_failure_is_unexpected(
            Some(128),
            "Fatal: Not a git repository (or any of the parent directories): .git"
        ));
    }

    proptest::proptest! {
        /// A "not a git repository" message in any case, at any place in stderr, is never
        /// unexpected with git's fatal exit (#3055, #3433).
        #[test]
        fn a_non_repo_message_in_any_case_is_never_unexpected(
            code in proptest::strategy::Just(Some(128)),
            before in ".{0,40}",
            message in "[nN][oO][tT] [aA] [gG][iI][tT] [rR][eE][pP][oO][sS][iI][tT][oO][rR][yY]",
            after in ".{0,40}",
        ) {
            let stderr = format!("{before}{message}{after}");
            proptest::prop_assert!(!branch_failure_is_unexpected(code, &stderr), "{stderr}");
        }

        /// Over every exit code and stderr: a silent exit 1 and exit 128 with the phrase are
        /// expected, and every other failure is unexpected.
        #[test]
        fn only_a_silent_exit_1_or_a_folder_outside_git_is_expected(
            code in proptest::option::of(-1i32..300),
            stderr in proptest::prop_oneof![
                proptest::strategy::Just(String::new()),
                proptest::strategy::Just(" \n".to_string()),
                proptest::strategy::Just("fatal: not a git repository".to_string()),
                "[a-z :]{1,30}",
            ],
        ) {
            let silent_detached = code == Some(1) && stderr.trim().is_empty();
            let outside = code == Some(128) && stderr.contains("not a git repository");
            proptest::prop_assert_eq!(
                branch_failure_is_unexpected(code, &stderr),
                !silent_detached && !outside
            );
        }
    }

    // #3433: the phrase does not hide a signal kill or another exit code.
    #[test]
    fn the_non_repo_phrase_is_expected_only_with_exit_128() {
        let phrase = "fatal: not a git repository (or any of the parent directories): .git";
        assert!(!branch_failure_is_unexpected(Some(128), phrase));
        assert!(branch_failure_is_unexpected(None, phrase));
        assert!(branch_failure_is_unexpected(Some(2), phrase));
        assert!(branch_failure_is_unexpected(
            Some(1),
            "error: cannot lock ref"
        ));
        assert!(!branch_failure_is_unexpected(Some(1), ""));
    }

    #[test]
    fn branch_failure_is_unexpected_flags_ownership_and_signal_failures() {
        assert!(branch_failure_is_unexpected(
            Some(128),
            "fatal: detected dubious ownership in repository at '/x'"
        ));
        assert!(branch_failure_is_unexpected(None, ""));
        assert!(branch_failure_is_unexpected(Some(2), "usage: git"));
    }
}
