//! Warn-only `PreToolUse` advisory for a Bash command that ends with `cd` (#976, #2900).
//!
//! In the main agent, the Bash tool keeps its working directory between calls, so a command
//! that ends with `cd` changes where the next call runs. In a subagent the directory can differ.
//! A `cd` followed by more commands in the same call (`cd /x && cargo test`) changes nothing
//! after the call, so it gets no advisory.
//!
//! Stateless, unlike `read_once`/`repeat_detect`: it only inspects the
//! current call's `tool_input.command`, no per-session tracking needed.

use crate::config::CdGuard;
use crate::hook_run::shell_split::{
    NestingTooDeep, ends_with_substitution, program_words, shell_segments,
};

const ADVISORY: &str = "note: this command ends with `cd`, so the next Bash call starts in \
that directory in the main agent, and a subagent can start elsewhere. Run the commands that need \
the directory in the same call (`cd /x && cmd`), or use absolute paths.";

/// Handle a `PreToolUse` event for the cd-guard feature. Returns the
/// advisory text, or an empty string when the guard doesn't apply (disabled,
/// not a Bash call, or no `cd` detected).
pub fn handle_pre_tool_use(stdin_payload: &serde_json::Value, cfg: &CdGuard) -> String {
    if !cfg.enabled {
        return String::new();
    }
    let Some("Bash") = stdin_payload.get("tool_name").and_then(|v| v.as_str()) else {
        return String::new();
    };
    let Some(command) = stdin_payload
        .get("tool_input")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
    else {
        // Claude Code's Bash tool schema guarantees `command` is present as a
        // string — this should never fire in practice. Log it rather than
        // silently no-op, in case the harness's payload shape ever drifts.
        // `error!`, not `warn!`/`debug!`: the default `EnvFilter` (`RUST_LOG`
        // unset) is ERROR-only and drops anything weaker before it reaches a
        // default-configured user's log (#1133 precedent).
        tracing::error!("tool_input.command missing or not a string for Bash PreToolUse payload");
        return String::new();
    };
    if command_ends_with_cd(command) {
        ADVISORY.to_string()
    } else {
        String::new()
    }
}

/// The words that close a brace group or a compound command. The group runs in the current
/// shell, so a `cd` just before the closer still outlives the call.
const CLOSERS: [&str; 4] = ["}", "fi", "done", "esac"];

/// Whether the last command of a call sits before this segment. A bare closer ends a group or a
/// compound command, and an `eval` segment comes after the commands it runs. A closer with a
/// word after it is a command of its own, so it is not transparent (`fi a` runs `a`).
fn is_transparent(words: &[String]) -> bool {
    let program = program_words(words);
    let bare_closer = program.len() == 1 && CLOSERS.contains(&program[0]);
    bare_closer || program.first() == Some(&"eval")
}

/// The words that open a branch or a loop body ahead of the command they run.
const OPENERS: [&str; 3] = ["then", "do", "else"];

/// Whether `words` runs `cd` in the shell of the call. A `cd` in a `( ... )` subshell ends with the
/// subshell, so it does not count.
fn runs_cd_in_this_shell(words: &[String]) -> bool {
    if words.first().is_some_and(|w| w.starts_with('(')) {
        return false;
    }
    let program = program_words(words);
    let start = program.iter().take_while(|w| OPENERS.contains(w)).count();
    program.get(start) == Some(&"cd")
}

/// Whether the last command of `command` is `cd`, so the change of directory outlives the call.
///
/// The parts come from the shared splitter that the commit gate uses (#2619). It puts each
/// substitution ahead of the command that runs it, so the last part is the last command at the
/// top level. A command nested too deeply to split gets no advisory, and the parse failure is
/// logged instead.
fn command_ends_with_cd(command: &str) -> bool {
    match shell_segments(command).and_then(|s| Ok((s, ends_with_substitution(command)?))) {
        Ok((_, true)) => false,
        Ok((segments, false)) => segments
            .iter()
            .rev()
            .find(|words| !is_transparent(words))
            .is_some_and(|words| runs_cd_in_this_shell(words)),
        Err(NestingTooDeep) => {
            tracing::error!("cd guard: command nests too deep to check for cd; no advisory");
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bash_payload(command: &str) -> serde_json::Value {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command },
        })
    }

    #[test]
    fn detects_bare_cd() {
        assert!(command_ends_with_cd("cd /tmp"));
    }

    #[test]
    fn a_cd_followed_by_more_commands_gets_no_advisory() {
        assert!(!command_ends_with_cd("cd /tmp && ls"));
        assert!(!command_ends_with_cd("cd /tmp; cargo test"));
    }

    #[test]
    fn a_closer_with_a_word_after_it_runs_that_word() {
        assert!(!command_ends_with_cd("cd a && fi a"));
        assert!(command_ends_with_cd("cd a && fi"));
    }

    #[test]
    fn detects_trailing_cd_in_compound_and() {
        assert!(command_ends_with_cd("ls && cd /tmp"));
    }

    #[test]
    fn detects_cd_after_semicolon() {
        assert!(command_ends_with_cd("ls; cd /tmp"));
        assert!(command_ends_with_cd("ls; cd /tmp;"));
    }

    #[test]
    fn detects_cd_after_or() {
        assert!(command_ends_with_cd("false || cd /tmp"));
    }

    #[test]
    fn detects_cd_across_newlines() {
        assert!(command_ends_with_cd("ls\ncd /tmp"));
    }

    #[test]
    fn a_cd_inside_a_command_substitution_gets_no_advisory() {
        assert!(!command_ends_with_cd("echo $(cd /tmp && ls)"));
    }

    #[test]
    fn a_cd_inside_a_subshell_group_gets_no_advisory() {
        assert!(!command_ends_with_cd("(cd /tmp && ls)"));
    }

    #[test]
    fn a_cd_inside_a_brace_group_followed_by_a_command_gets_no_advisory() {
        assert!(!command_ends_with_cd("{ cd /tmp; ls; }"));
    }

    #[test]
    fn a_trailing_builtin_cd_gets_the_advisory() {
        assert!(command_ends_with_cd("ls && builtin cd /tmp"));
        assert!(command_ends_with_cd("command cd /tmp"));
    }

    #[test]
    fn a_trailing_cd_in_a_subshell_gets_no_advisory() {
        assert!(!command_ends_with_cd("ls && (cd /tmp)"));
        assert!(!command_ends_with_cd("(cd /tmp)"));
    }

    #[test]
    fn a_trailing_cd_in_a_group_or_compound_command_gets_the_advisory() {
        for command in [
            "{ cd /tmp; }",
            "ls; { cd /tmp; }",
            "if true; then cd /x; fi",
            "for d in /x; do cd $d; done",
            "if false; then ls; else cd /x; fi",
        ] {
            assert!(command_ends_with_cd(command), "{command}");
        }
    }

    #[test]
    fn a_trailing_cd_in_a_substitution_gets_no_advisory() {
        for command in [
            "$(cd /tmp)",
            "ls && $(cd /tmp)",
            "`cd /tmp`",
            "ls; $(cd /tmp)  ",
        ] {
            assert!(!command_ends_with_cd(command), "{command}");
        }
    }

    #[test]
    fn a_cd_to_a_substituted_path_gets_the_advisory() {
        assert!(command_ends_with_cd("cd $(git rev-parse --show-toplevel)"));
        assert!(command_ends_with_cd("cd \"$(pwd)\""));
    }

    #[test]
    fn a_cd_that_eval_runs_gets_the_advisory() {
        assert!(command_ends_with_cd("eval cd /x"));
        assert!(command_ends_with_cd("ls && eval \"cd /x\""));
        assert!(!command_ends_with_cd("cd /x; eval true"));
    }

    #[test]
    fn a_command_too_deep_to_split_gets_no_cd_advisory() {
        let nested = format!("{}cd /tmp{}", "$(".repeat(40), ")".repeat(40));
        assert!(!command_ends_with_cd(&nested));
    }

    #[test]
    fn ignores_command_without_cd() {
        assert!(!command_ends_with_cd("ls -la /tmp"));
    }

    #[test]
    fn ignores_cd_as_a_substring_of_another_word() {
        assert!(!command_ends_with_cd("mkdir abcd && ls"));
    }

    #[test]
    fn ignores_cdpath_env_assignment() {
        assert!(!command_ends_with_cd("CDPATH=/tmp ls"));
    }

    #[test]
    fn handle_pre_tool_use_returns_advisory_for_a_trailing_cd() {
        let text = handle_pre_tool_use(&bash_payload("ls && cd /tmp"), &CdGuard { enabled: true });
        assert_eq!(text, ADVISORY);
        assert!(
            !text.contains("resets"),
            "Claude Code keeps the directory: {text}"
        );
        let none = handle_pre_tool_use(
            &bash_payload("cd /x && cargo test"),
            &CdGuard { enabled: true },
        );
        assert!(none.is_empty());
    }

    #[test]
    fn handle_pre_tool_use_empty_when_disabled() {
        let text = handle_pre_tool_use(&bash_payload("cd /tmp"), &CdGuard { enabled: false });
        assert!(text.is_empty());
    }

    #[test]
    fn handle_pre_tool_use_empty_for_non_bash_tool() {
        let payload = serde_json::json!({
            "tool_name": "Read",
            "tool_input": { "file_path": "/tmp/x" },
        });
        let text = handle_pre_tool_use(&payload, &CdGuard { enabled: true });
        assert!(text.is_empty());
    }

    #[test]
    fn handle_pre_tool_use_empty_when_no_cd() {
        let text = handle_pre_tool_use(&bash_payload("ls -la"), &CdGuard { enabled: true });
        assert!(text.is_empty());
    }

    // #1207: a Bash payload missing tool_input.command should never happen in
    // practice (Claude Code's schema guarantees it), but if the harness's
    // payload shape ever drifts, cd_guard would otherwise stop firing with no
    // trace in the logs.
    #[test]
    fn handle_pre_tool_use_logs_error_when_bash_command_missing() {
        let payload = serde_json::json!({ "tool_name": "Bash", "tool_input": {} });
        let logs = crate::test_log_capture::capture_logs(|| {
            let text = handle_pre_tool_use(&payload, &CdGuard { enabled: true });
            assert!(text.is_empty());
        });
        assert!(
            logs.contains("tool_input.command missing"),
            "expected an error log when tool_input.command is absent, got: {logs}"
        );
        assert!(
            logs.contains("ERROR"),
            "must log at error level so it survives the default (RUST_LOG-unset) filter, got: {logs}"
        );
    }

    proptest! {
        // A command that ends with a substitution and nothing else never gets the advisory, and
        // the text before it decides nothing (#2937).
        #[test]
        fn a_trailing_substitution_is_found_and_ends_the_check(
            prefix in proptest::collection::vec("[a-z]{1,6}", 0..4),
            inner in "[a-z]{1,6}( /[a-z]{1,6})?",
            backtick in proptest::bool::ANY,
        ) {
            let tail = if backtick { format!("`{inner}`") } else { format!("$({inner})") };
            let command = if prefix.is_empty() {
                tail
            } else {
                format!("{} && {tail}", prefix.join(" && "))
            };
            prop_assert_eq!(
                crate::hook_run::shell_split::ends_with_substitution(&command),
                Ok(true)
            );
            prop_assert!(!command_ends_with_cd(&command), "{}", command);
        }

        #[test]
        fn a_command_that_ends_in_plain_text_has_no_trailing_substitution(
            command in "[a-z $()&;|]{0,30}[a-z]",
        ) {
            prop_assert_eq!(
                crate::hook_run::shell_split::ends_with_substitution(&command),
                Ok(false)
            );
        }
    }

    proptest! {
        // Arbitrary text never panics, regardless of shell-metacharacter content.
        #[test]
        fn command_ends_with_cd_never_panics(command in ".{0,200}") {
            let _ = command_ends_with_cd(&command);
        }
    }

    // #1208: strengthen coverage beyond panic-safety with real correctness
    // invariants over generated compound-command shapes.
    mod correctness_proptests {
        use super::*;

        /// A token that never equals `cd` exactly, safe to use as a
        /// non-cd leading word or argument (no internal whitespace).
        fn arb_non_cd_token() -> impl Strategy<Value = String> {
            "[a-zA-Z][a-zA-Z0-9_]{0,8}".prop_filter("must not be exactly 'cd'", |s| s != "cd")
        }

        fn arb_separator() -> impl Strategy<Value = &'static str> {
            proptest::sample::select(&["&&", "||", ";", "\n", "|"][..])
        }

        /// Builds a compound command from 1-5 segments, each
        /// `<preceding separator> <leading> <arg>` (the first segment's
        /// separator is generated but unused) — plus the expected
        /// `command_ends_with_cd` result for it.
        fn arb_command_and_expected() -> impl Strategy<Value = (String, bool)> {
            let segment = (
                proptest::bool::ANY,
                arb_non_cd_token(),
                arb_non_cd_token(),
                arb_separator(),
            );
            proptest::collection::vec(segment, 1..6).prop_map(|segments| {
                let expected = segments
                    .last()
                    .is_some_and(|&(leads_with_cd, ..)| leads_with_cd);
                let mut command = String::new();
                for (i, (leads_with_cd, token, arg, sep)) in segments.iter().enumerate() {
                    if i > 0 {
                        command.push(' ');
                        command.push_str(sep);
                        command.push(' ');
                    }
                    command.push_str(if *leads_with_cd { "cd" } else { token });
                    command.push(' ');
                    command.push_str(arg);
                }
                (command, expected)
            })
        }

        /// A token containing `cd` as a substring but never equal to it
        /// exactly — e.g. `abcd`, `cdx` — used to prove substring/argument
        /// occurrences of `cd` don't trigger a false positive.
        fn arb_cd_like_but_not_exact_token() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_]{0,4}cd[a-zA-Z0-9_]{0,4}"
                .prop_filter("must not be exactly 'cd'", |s| s != "cd")
        }

        proptest! {
            #[test]
            fn command_ends_with_cd_matches_leading_cd_segments((command, expected) in arb_command_and_expected()) {
                prop_assert_eq!(command_ends_with_cd(&command), expected, "command: {:?}", command);
            }

            #[test]
            fn command_ends_with_cd_ignores_non_leading_cd_occurrences(
                leading in arb_cd_like_but_not_exact_token(),
                trailing in proptest::sample::select(vec!["cd", "cd/", "xcd", "cdx"]),
            ) {
                let command = format!("{leading} {trailing}");
                prop_assert!(!command_ends_with_cd(&command), "command: {:?}", command);
            }
        }
    }
}
