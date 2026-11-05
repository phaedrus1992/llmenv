//! Classifies a failed `git symbolic-ref` run for the task tracker (#3040).

/// Whether a failed `git symbolic-ref --quiet --short HEAD` is worth telling the user about.
/// Exit 1 is a detached HEAD, and exit 128 with "not a git repository" is a directory outside
/// git. Both are normal. Anything else, such as "dubious ownership", a kill by signal, or the
/// phrase from another exit code, hides the branch for a reason the user can fix (#3433).
pub(super) fn branch_failure_is_unexpected(code: Option<i32>, stderr: &str) -> bool {
    let outside_git = code == Some(GIT_FATAL_EXIT) && crate::git::is_not_a_repo_message(stderr);
    code != Some(1) && !outside_git
}

/// The exit code of a git command that stops on a fatal error, such as a folder outside git.
const GIT_FATAL_EXIT: i32 = 128;

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

        /// Exit 1 is a detached HEAD, which is never unexpected, whatever stderr holds.
        #[test]
        fn exit_one_is_never_unexpected(stderr in ".{0,80}") {
            proptest::prop_assert!(!branch_failure_is_unexpected(Some(1), &stderr));
        }
    }

    // #3433: the phrase does not hide a signal kill or another exit code.
    #[test]
    fn the_non_repo_phrase_is_expected_only_with_exit_128() {
        let phrase = "fatal: not a git repository (or any of the parent directories): .git";
        assert!(!branch_failure_is_unexpected(Some(128), phrase));
        assert!(branch_failure_is_unexpected(None, phrase));
        assert!(branch_failure_is_unexpected(Some(2), phrase));
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
