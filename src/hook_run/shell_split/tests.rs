use super::*;
use proptest::prelude::*;

#[test]
fn takes_value_reads_exact_options_and_short_clusters() {
    let opts = wrappers::TIMEOUT_OPTS;
    assert!(takes_value("-s", opts));
    assert!(takes_value("--signal", opts));
    // A cluster that ends in a value option takes the next word.
    assert!(takes_value("-ks", opts));
    // An attached value takes none.
    assert!(!takes_value("-sKILL", opts));
    assert!(!takes_value("--signal=KILL", opts));
    // A long option is never a cluster, even when its last letter is a value option.
    assert!(!takes_value("--x-s", opts));
    assert!(!takes_value("-x", opts));
    assert!(!takes_value("-", opts));
}

#[test]
fn step_boundary_names_a_push_or_a_pull_request_step() {
    assert_eq!(boundary("git push origin HEAD"), Some("git push"));
    assert_eq!(boundary("git -C /repo push"), Some("git push"));
    assert_eq!(boundary("gh pr create --title x"), Some("gh pr create"));
    assert_eq!(boundary("gh pr ready 12"), Some("gh pr ready"));
    assert_eq!(
        boundary("gh -R o/r pr merge 3 --merge"),
        Some("gh pr merge")
    );
}

#[test]
fn step_boundary_ignores_other_git_and_gh_calls() {
    assert_eq!(boundary("git status"), None);
    assert_eq!(boundary("git commit -m x"), None);
    assert_eq!(boundary("gh pr view 3"), None);
    assert_eq!(boundary("gh issue list"), None);
    assert_eq!(boundary("echo git push"), None);
}

#[test]
fn step_boundary_names_the_first_boundary_of_a_chain() {
    assert_eq!(
        boundary("git commit -m x && git push && gh pr merge 1"),
        Some("git push")
    );
}

/// The boundary of one command, or `None` when it has none or cannot be split.
fn boundary(command: &str) -> Option<&'static str> {
    step_boundary(command).ok().flatten()
}

#[test]
fn a_command_too_deep_to_split_has_no_boundary_and_says_so() {
    let nested = format!("{}git push{}", "$(".repeat(40), ")".repeat(40));
    assert!(step_boundary(&nested).is_err());
}

proptest! {
    /// A global `-R <repo>` or `--repo=<repo>` before the subcommand never changes the pair.
    #[test]
    fn a_repo_flag_never_changes_the_gh_subcommands(repo in "[a-z]{1,8}", long in any::<bool>()) {
        let mut owned: Vec<String> = if long {
            vec![format!("--repo={repo}")]
        } else {
            vec!["-R".to_string(), repo]
        };
        owned.extend(["pr", "create"].iter().map(|w| w.to_string()));
        let words: Vec<&str> = owned.iter().map(String::as_str).collect();
        prop_assert_eq!(gh_subcommands(&words), vec!["pr", "create"]);
    }

    /// A git option value is skipped even when it spells a subcommand.
    #[test]
    fn a_git_option_value_is_never_the_subcommand(
        value in prop::sample::select(vec!["push", "commit", "status", "x"]),
        flag in prop::sample::select(vec!["-c", "-C", "--git-dir", "--work-tree"]),
        sub in prop::sample::select(vec!["push", "commit", "status"]),
    ) {
        let words = [flag, value, sub];
        prop_assert_eq!(git_subcommand(&words), Some(sub));
    }

    /// Any text, shell metacharacters included, gives a result and never panics.
    #[test]
    fn step_boundary_never_panics(command in "[a-z$(){}|&;'\" =-]{0,80}") {
        let _ = step_boundary(&command);
    }

    /// A shell prefix does not hide a push from the boundary check.
    #[test]
    fn a_wrapper_before_git_push_still_names_the_push(
        prefix in prop::sample::select(vec!["", "env ", "sudo ", "FOO=1 ", "( ", "{ "]),
    ) {
        let command = format!("{prefix}git push origin HEAD");
        prop_assert_eq!(boundary(&command), Some("git push"));
    }
}
