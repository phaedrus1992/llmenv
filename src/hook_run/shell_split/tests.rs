use super::*;

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
    assert_eq!(step_boundary("git push origin HEAD"), Some("git push"));
    assert_eq!(step_boundary("git -C /repo push"), Some("git push"));
    assert_eq!(
        step_boundary("gh pr create --title x"),
        Some("gh pr create")
    );
    assert_eq!(step_boundary("gh pr ready 12"), Some("gh pr ready"));
    assert_eq!(
        step_boundary("gh -R o/r pr merge 3 --merge"),
        Some("gh pr merge")
    );
}

#[test]
fn step_boundary_ignores_other_git_and_gh_calls() {
    assert_eq!(step_boundary("git status"), None);
    assert_eq!(step_boundary("git commit -m x"), None);
    assert_eq!(step_boundary("gh pr view 3"), None);
    assert_eq!(step_boundary("gh issue list"), None);
    assert_eq!(step_boundary("echo git push"), None);
}

#[test]
fn step_boundary_names_the_first_boundary_of_a_chain() {
    assert_eq!(
        step_boundary("git commit -m x && git push && gh pr merge 1"),
        Some("git push")
    );
}
