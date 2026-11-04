use super::*;

#[test]
fn nested_shells_and_evals_fail_closed_past_the_depth_bound() {
    let depth = MAX_SUBSTITUTION_DEPTH + 8;
    let eval_chain = format!("{}git commit -m x", "eval ".repeat(depth));
    assert!(runs_commit_or_pr(&eval_chain));
    assert!(!runs_commit_or_pr(&format!("{}echo hi", "eval ".repeat(2))));
    assert!(runs_commit_or_pr("sh -c 'sh -c \"git commit -m x\"'"));
    assert!(runs_commit_or_pr("bash -lc \"eval 'git commit'\""));
    // The splitter stops at the bound, so a long chain returns instead of growing the stack.
    let long_chain = format!("{}git commit", "eval ".repeat(20_000));
    assert!(runs_commit_or_pr(&long_chain));
}

#[test]
fn commit_and_pull_request_commands_are_found_in_any_part_of_a_command() {
    for (command, expected) in [
        ("git commit -m x", true),
        ("git add -A && git commit -m x", true),
        ("cd repo; git commit", true),
        ("git -C repo commit -m x", true),
        ("git -c user.name=a commit", true),
        ("GIT_AUTHOR_NAME=a git commit", true),
        ("git status\ngit commit", true),
        ("gh pr create --title x", true),
        ("true || gh pr create", true),
        ("cat <(git commit -m x)", true),
        ("tee >(git commit -m x)", true),
        ("echo \"<(git commit -m x)\"", false),
        ("echo '<(git commit -m x)'", false),
        ("git status", false),
        ("git log --oneline", false),
        ("echo \"git commit\"", false),
        ("gh pr view 1", false),
        ("gh issue create", false),
        ("git commit-tree abc", false),
        ("", false),
        ("FOO=\"a b\" git commit -m x", true),
        ("env git commit", true),
        ("command git commit", true),
        ("/usr/bin/git commit -m x", true),
        ("(git commit -m x)", true),
        ("{ git commit; }", true),
        ("eval \"git commit -m x\"", true),
        ("eval git commit -m x", true),
        ("timeout 60 git commit", true),
        ("timeout -s KILL 60 git commit", true),
        ("env -i git commit", true),
        ("env -u HOME git commit", true),
        ("nice -n 5 git commit", true),
        ("sudo -u root git commit", true),
        ("xargs -n 1 git commit", true),
        ("stdbuf -oL git commit", true),
        ("setsid git commit", true),
        ("eval \"echo hi\"", false),
        ("timeout 60 git status", false),
        ("env -i git status", false),
        ("env -S 'git commit -m x'", true),
        ("env -i -S 'git commit'", true),
        ("FOO=1 env -S 'git commit'", true),
        ("env -u HOME -S 'git commit'", true),
        ("env FOO=1 -S 'git commit'", true),
        ("env --split-string='git commit'", true),
        ("env '-Sgit commit'", true),
        ("env git commit -S", true),
        ("timeout --signal KILL 60 git commit", true),
        ("timeout --kill-after=5 60 git commit", true),
        ("timeout -ks 5 60 git commit", true),
        ("gtimeout 60 git commit", true),
        ("gnice -n 5 git commit", true),
        ("sudo -r role git commit", true),
        ("sudo -t sudoers git commit", true),
        ("flock /tmp/lock git commit", true),
        ("flock -w 5 /tmp/lock git commit", true),
        ("git -C repo status", false),
        ("bash -c \"git add . && git commit -m x\"", true),
        ("gh -R owner/repo pr create", true),
        ("gh pr create --title \"a && b\"", true),
        ("echo \"x; git commit\"", false),
        ("echo 'a && gh pr create'", false),
        ("git commit -m \"fix: a; b\"", true),
        ("git status 2>&1", false),
        ("gh --no-pager pr create", true),
        ("git --no-pager commit -m x", true),
        ("gh --no-pager pr view 1", false),
        ("bash script.sh", false),
        ("bash -lc \"git commit -m x\"", true),
        ("bash -ec 'git commit -m x'", true),
        ("sh -l -c \"git commit\"", true),
        ("bash --noclobber -c 'echo hi'", false),
        ("bash -lc 'echo hi'", false),
        ("bash -c", false),
        ("echo $(git commit -m x)", true),
        ("echo `git commit -m x`", true),
        ("echo \"$(git commit -m x)\"", true),
        ("echo $(git status) && git commit", true),
        ("echo $(echo $(git commit))", true),
        ("echo '$(git commit)'", false),
        ("echo $(git status)", false),
        ("echo \"it's\" && $(gh pr create)", true),
        ("echo $((1 + 2))", false),
        ("bash -o pipefail -c \"git commit -m x\"", true),
        ("bash -euo pipefail -c 'git commit -m x'", true),
        ("bash -O extglob -c 'git commit -m x'", true),
        ("bash +x -c 'git commit -m x'", true),
        ("bash --rcfile f -c 'git commit -m x'", true),
        ("bash -o pipefail script.sh", false),
        ("echo \\'$(git commit -m x)", true),
        ("echo \"\\$(git commit)\"", false),
    ] {
        assert_eq!(runs_commit_or_pr(command), expected, "{command:?}");
    }
}

#[test]
fn workflow_skills_match_without_the_plugin_prefix() {
    let tracker = TaskTracker::default();
    assert!(is_workflow_skill(&tracker, "dev-sprint"));
    assert!(is_workflow_skill(&tracker, "nbl-dev:ship-issue"));
    assert!(!is_workflow_skill(&tracker, "brainstorming"));
    let custom = TaskTracker {
        workflow_skills: Some(vec!["my:deploy".to_string()]),
        ..TaskTracker::default()
    };
    assert!(is_workflow_skill(&custom, "deploy"));
    assert!(!is_workflow_skill(&custom, "dev-sprint"));
}

#[test]
fn a_session_id_that_is_not_a_plain_name_has_no_state_file() {
    let dir = Path::new("/state");
    assert!(state_path(dir, "abc-123").is_some());
    for bad in ["", "../x", "a/b", ".."] {
        assert_eq!(state_path(dir, bad), None, "{bad:?}");
    }
}

proptest! {
    #[test]
    fn text_without_a_substitution_is_left_unchanged(s in "[a-z ;|&()'\"\\\\]{0,40}") {
        prop_assert_eq!(split_substitutions(&s).unwrap(), s);
    }

    #[test]
    fn split_substitutions_never_panics_on_substitution_syntax(
        s in "[a-z $`<>()'\"\\\\;|&\n]{0,40}",
    ) {
        let _ = split_substitutions(&s);
    }

    #[test]
    fn a_live_process_substitution_becomes_a_part(
        prefix in "[a-z ]{0,10}",
        inner in "[a-z ]{0,10}",
        kind in prop::sample::select(vec!["<", ">"]),
    ) {
        let command = format!("{prefix}{kind}({inner})");
        prop_assert_eq!(split_substitutions(&command).unwrap(), format!("{prefix};{inner};"));
    }

    #[test]
    fn a_process_substitution_inside_double_quotes_is_literal(
        prefix in "[a-z ]{0,10}",
        inner in "[a-z ]{0,10}",
        kind in prop::sample::select(vec!["<", ">"]),
    ) {
        let command = format!("\"{prefix}{kind}({inner})\"");
        prop_assert_eq!(split_substitutions(&command).unwrap(), command);
    }

    #[test]
    fn take_until_close_stops_at_the_first_unnested_paren(
        s in "[a ;]{0,20}",
        tail in "[a )]{0,10}",
    ) {
        let text = format!("{s}){tail}");
        let mut chars = text.chars().peekable();
        prop_assert_eq!(take_until_close(&mut chars), s);
        prop_assert_eq!(chars.collect::<String>(), tail);
    }
}

#[test]
fn split_substitutions_rewrites_only_live_substitutions() {
    for (input, expected) in [
        ("echo $(a)", ";a;echo "),
        ("echo `a`", ";a;echo "),
        ("echo '$(a)'", "echo '$(a)'"),
        ("echo '`a`'", "echo '`a`'"),
        ("echo \\$(a)", "echo \\$(a)"),
        ("echo \\'$(a)", ";a;echo \\'"),
        ("echo \"\\$(a)\"", "echo \"\\$(a)\""),
        ("echo \"'\" $(a)", ";a;echo \"'\" "),
        ("echo '\"' $(a)", ";a;echo '\"' "),
        ("echo $HOME", "echo $HOME"),
        ("cat <(a)", "cat ;a;"),
        ("tee >(a)", "tee ;a;"),
        ("echo \"<(a)\"", "echo \"<(a)\""),
        ("echo '>(a)'", "echo '>(a)'"),
        // A separator inside quotes is text, not a segment boundary.
        ("echo 'a;b' $(c)", ";c;echo 'a;b' "),
        ("echo \"a;b\" $(c)", ";c;echo \"a;b\" "),
        ("echo \"$(c)\"", ";c;echo \"\""),
        // In single quotes a backslash is literal, so the quote after it closes the string.
        ("echo 'a\\' $(a)", ";a;echo 'a\\' "),
    ] {
        assert_eq!(split_substitutions(input).unwrap(), expected, "{input:?}");
    }
}

#[test]
fn take_until_close_counts_nested_parens() {
    for (text, inner, rest) in [
        ("a)b", "a", "b"),
        ("a(b)c)d", "a(b)c", "d"),
        ("(x)(y))tail", "(x)(y)", "tail"),
        ("abc", "abc", ""),
        // A quote hides the closing paren, and a close quote ends the hiding.
        ("echo 'a)b' c)rest", "echo 'a)b' c", "rest"),
        ("echo \\) x)rest", "echo \\) x", "rest"),
        ("echo \"'\" ) rest", "echo \"'\" ", " rest"),
        ("echo \")\" x)tail", "echo \")\" x", "tail"),
        ("echo \"(\" ) x)tail", "echo \"(\" ", " x)tail"),
        // A `)` inside quotes does not close a nested `(`.
        ("a(\")\"b) rest)tail", "a(\")\"b) rest", "tail"),
    ] {
        let mut chars = text.chars().peekable();
        assert_eq!(take_until_close(&mut chars), inner, "{text:?}");
        assert_eq!(chars.collect::<String>(), rest, "{text:?}");
    }
}

#[test]
fn a_program_other_than_llmenv_with_the_same_shape_does_not_end_a_task() {
    assert!(!ends_task_before_commit(
        "echo task done step-one && git commit -m x",
        "step-one"
    ));
}

/// Prefixes that the commit and task-end matchers must see through.
const WRAPPER_PREFIXES: [&str; 7] = ["", "env ", "sudo ", "(", "{ ", "FOO=1 ", "nohup "];

proptest! {
    #[test]
    fn plain_words_form_one_segment(words in proptest::collection::vec("[a-z0-9-]{1,8}", 1..6)) {
        prop_assert_eq!(shell_segments(&words.join(" ")), Ok(vec![words]));
    }

    #[test]
    fn an_operator_inside_single_quotes_never_splits(inner in "[a-z;|&]{0,6}") {
        let command = format!("echo '{inner}' tail");
        prop_assert_eq!(shell_segments(&command).unwrap().len(), 1);
    }

    #[test]
    fn a_wrapper_before_git_commit_is_still_a_commit(
        prefix in proptest::sample::select(WRAPPER_PREFIXES.to_vec()),
    ) {
        let command = format!("{prefix}git commit -m x");
        prop_assert!(runs_commit_or_pr(&command));
    }

    #[test]
    fn a_wrapper_before_a_task_end_is_still_a_task_end(
        prefix in proptest::sample::select(WRAPPER_PREFIXES.to_vec()),
    ) {
        let command = format!("{prefix}llmenv task done step-one; git commit -m x");
        prop_assert!(ends_task_before_commit(&command, "step-one"));
    }

    #[test]
    fn nudge_state_survives_a_json_roundtrip(
        calls in any::<u32>(),
        last_nudge in any::<u32>(),
        skill_reminded in any::<bool>(),
        commit_denied in any::<bool>(),
        start_reminded in proptest::option::of("[a-z-]{1,12}"),
        boundary_reminded in proptest::collection::vec("[a-z -]{1,20}", 0..4),
        stop_quiet_turns in any::<u32>(),
    ) {
        let state = NudgeState {
            calls,
            last_nudge,
            skill_reminded,
            commit_denied,
            start_reminded,
            boundary_reminded,
            stop_quiet_turns,
        };
        let json = serde_json::to_string(&state).unwrap();
        prop_assert_eq!(serde_json::from_str::<NudgeState>(&json).unwrap(), state);
    }

    #[test]
    fn a_command_without_commit_or_pr_create_is_never_matched(command in "[a-z0-9 -;|&\n]{0,60}") {
        prop_assume!(!command.contains("commit") && !command.contains("pr create"));
        prop_assert!(!runs_commit_or_pr(&command));
    }

    #[test]
    fn matching_never_panics(command in "\\PC{0,80}") {
        let _ = runs_commit_or_pr(&command);
    }
}
