//! The llmenv programs that the rendered Claude Code hook commands name (#3392).
//!
//! An earlier `llmenv regenerate` can write a path that the running doctor does not share, such
//! as a cargo target binary, so the doctor check reads the written commands, not its own path.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The llmenv subcommands that a built-in hook or the status line runs. A test reads every
/// `hook_command("...")` call in the adapters, so a new subcommand cannot be left out.
const LLMENV_SUBCOMMANDS: [&str; 6] = [
    "hook-run",
    "config-guard",
    "config-context",
    "check-stale",
    "throttle",
    "statusline",
];

/// What the llmenv commands in a rendered config name.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RenderedPrograms {
    /// The distinct programs of the llmenv commands.
    pub(super) programs: BTreeSet<PathBuf>,
    /// llmenv commands whose program cannot be read as one shell word, as written.
    pub(super) unreadable: Vec<String>,
}

/// One command: an llmenv command with its program, an llmenv command that cannot be read, or
/// another command.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Llmenv(PathBuf),
    Unreadable,
    Other,
}

fn classify(command: &str) -> Command {
    let Ok(words) = shell_words::split(command) else {
        // A command that does not split still counts when it names an llmenv subcommand, so a
        // risky path cannot hide behind a broken quote.
        let names_llmenv = command
            .split_whitespace()
            .any(|w| LLMENV_SUBCOMMANDS.contains(&w));
        return if names_llmenv {
            Command::Unreadable
        } else {
            Command::Other
        };
    };
    match skip_env_prefix(command, &words) {
        [program, subcommand, ..] if LLMENV_SUBCOMMANDS.contains(&subcommand.as_str()) => {
            if program.is_empty() {
                Command::Unreadable
            } else {
                Command::Llmenv(PathBuf::from(program))
            }
        }
        _ => Command::Other,
    }
}

/// The words after a leading `env` and its `NAME=value` assignments. A hook command can start with
/// them, so the program is the first word after they are skipped (#3509).
///
/// The prefix is read from the raw, unquoted tokens of `command`. A quoted word such as `'a='` is a
/// command name to the shell, not an assignment, and the split words cannot show the quotes.
fn skip_env_prefix<'a>(command: &str, words: &'a [String]) -> &'a [String] {
    let raw = raw_words(command);
    // After `env`, its words are argv, so quoting does not change what `env` reads. A bare prefix
    // is shell syntax: a quoted word is a command name, not an assignment. The raw text tells the
    // two apart only while it lines up with the split words, which fails when a quoted value
    // holds a space. Then the split words are the only record.
    let env_argv = words.first().is_some_and(|w| w == "env");
    let skipped = if env_argv || raw.len() != words.len() {
        words
            .iter()
            .take_while(|word| word.as_str() == "env" || is_assignment(word))
            .count()
    } else {
        raw.iter()
            .take_while(|token| **token == "env" || is_assignment(token))
            .count()
    };
    &words[skipped.min(words.len())..]
}

/// The whitespace-separated words of `command` as written, so a quoted word that holds a space or
/// a tab stays one word. A backslash escape is not read here, which is the one case the caller
/// falls back from.
fn raw_words(command: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in command.char_indices() {
        // A backslash escapes the next character, except inside single quotes.
        if escaped {
            escaped = false;
            start.get_or_insert(i);
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            start.get_or_insert(i);
            continue;
        }
        match (quote, c) {
            (None, c) if c.is_whitespace() => {
                if let Some(s) = start.take() {
                    out.push(&command[s..i]);
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                start.get_or_insert(i);
            }
            (Some(open), c) if c == open => quote = None,
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        out.push(&command[s..]);
    }
    out
}

/// Whether `word` is a shell `NAME=value` assignment, with a name that is a plain identifier.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The reason a `hooks` or `statusLine` value cannot be read, or `None` when it has a usable shape.
fn malformed_shape(settings: &serde_json::Value) -> Option<&'static str> {
    if settings.get("hooks").is_some_and(|h| !h.is_object()) {
        return Some("the \"hooks\" value is not an object of hook events");
    }
    if settings.get("statusLine").is_some_and(|s| !s.is_object()) {
        return Some("the \"statusLine\" value is not an object with a command");
    }
    None
}

/// Every hook command and the status line command in a rendered `settings.json`.
fn commands(settings: &serde_json::Value) -> Vec<&str> {
    let hooks = settings
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|events| events.values())
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(serde_json::Value::as_str));
    let status = settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(serde_json::Value::as_str);
    hooks.chain(status).collect()
}

/// The programs and the unreadable llmenv commands in `settings`.
pub(super) fn rendered_programs(settings: &serde_json::Value) -> RenderedPrograms {
    let mut out = RenderedPrograms::default();
    if let Some(reason) = malformed_shape(settings) {
        out.unreadable.push(reason.to_string());
    }
    for command in commands(settings) {
        match classify(command) {
            Command::Llmenv(program) => {
                out.programs.insert(program);
            }
            Command::Unreadable => out.unreadable.push(command.to_string()),
            Command::Other => {}
        }
    }
    out
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::util::shell_word;

    #[test]
    fn the_program_of_each_llmenv_hook_is_read_and_others_are_skipped() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{"hooks": [
                    {"type": "command", "command": "/opt/bin/llmenv hook-run --engine claude_code pre_tool_use"},
                    {"type": "command", "command": "'/my dir/target/debug/llmenv' config-guard --engine claude_code"},
                    {"type": "command", "command": "bash ~/hooks/guard.sh"},
                    {"type": "command", "command": "'/broken hook-run"},
                ]}],
                "Stop": [{"hooks": [{"type": "command", "command": "llmenv hook-run --engine claude_code stop"}]}],
            },
            "statusLine": {"type": "command", "command": "\"/tmp/x/llmenv\" statusline --color always"},
        });
        let found = rendered_programs(&settings);
        let programs: Vec<PathBuf> = found.programs.into_iter().collect();
        assert_eq!(
            programs,
            [
                PathBuf::from("/my dir/target/debug/llmenv"),
                PathBuf::from("/opt/bin/llmenv"),
                PathBuf::from("/tmp/x/llmenv"),
                PathBuf::from("llmenv"),
            ]
        );
        assert_eq!(found.unreadable, ["'/broken hook-run"]);
    }

    #[test]
    fn a_settings_file_with_no_hooks_names_no_program() {
        assert_eq!(
            rendered_programs(&serde_json::json!({})),
            RenderedPrograms::default()
        );
    }

    /// A `hooks` or `statusLine` value of the wrong shape is unreadable, not an absent hook, so
    /// doctor does not report "no llmenv hook command" for a file it could not read (#3509).
    #[test]
    fn a_malformed_hooks_or_status_line_is_unreadable() {
        let hooks = rendered_programs(&serde_json::json!({"hooks": "x"}));
        assert!(hooks.programs.is_empty(), "{hooks:?}");
        assert_eq!(hooks.unreadable.len(), 1, "{hooks:?}");
        let status = rendered_programs(&serde_json::json!({"statusLine": ["x"]}));
        assert_eq!(status.unreadable.len(), 1, "{status:?}");
    }

    /// An `env NAME=value` prefix does not hide the program of an llmenv hook command (#3509).
    #[test]
    fn an_env_prefix_does_not_hide_the_llmenv_program() {
        assert_eq!(
            classify("env CLAUDE_CONFIG_DIR=/x /opt/bin/llmenv hook-run"),
            Command::Llmenv(PathBuf::from("/opt/bin/llmenv"))
        );
        assert_eq!(
            classify("FOO=1 BAR=2 llmenv throttle"),
            Command::Llmenv(PathBuf::from("llmenv"))
        );
        assert_eq!(classify("env FOO=1 cat file"), Command::Other);
    }

    proptest::proptest! {
        /// A prefix of `NAME=value` words, quoted values with spaces included, does not hide the
        /// llmenv program that follows it (#3506).
        #[test]
        fn quoted_assignment_values_do_not_hide_the_program(
            values in proptest::collection::vec("[a-z ]{0,6}", 0..3),
        ) {
            let mut command = String::from("env");
            for (i, value) in values.iter().enumerate() {
                command.push_str(&format!(" 'X{i}={value}'"));
            }
            command.push_str(" /opt/bin/llmenv hook-run");
            prop_assert_eq!(classify(&command), Command::Llmenv(PathBuf::from("/opt/bin/llmenv")));
        }

        /// A plain identifier before `=` is an assignment, and any other name is not (#3506).
        #[test]
        fn is_assignment_accepts_identifiers_only(
            name in "[A-Za-z_][A-Za-z0-9_]{0,8}",
            value in "[ -~]{0,8}",
        ) {
            let plain = format!("{name}={value}");
            let digit_first = format!("1{name}={value}");
            let dashed = format!("a-{name}={value}");
            prop_assert!(is_assignment(plain.as_str()));
            prop_assert!(!is_assignment(digit_first.as_str()));
            prop_assert!(!is_assignment(dashed.as_str()));
        }
    }

    /// Each llmenv subcommand that the adapters write into a hook command is in the list.
    #[test]
    fn every_adapter_hook_command_uses_a_listed_subcommand() {
        let sources = [
            include_str!("../../adapter/claude_code/mod.rs"),
            include_str!("../../adapter/claude_code/seed.rs"),
            include_str!("../../adapter/crush.rs"),
            include_str!("../../adapter/opencode/hook_bridge.rs"),
        ];
        let mut seen = 0;
        for source in sources {
            for call in source.split("hook_command(\"").skip(1) {
                let subcommand = call.split([' ', '"']).next().unwrap();
                assert!(
                    LLMENV_SUBCOMMANDS.contains(&subcommand),
                    "hook_command(\"{subcommand} ...\") is not in LLMENV_SUBCOMMANDS"
                );
                seen += 1;
            }
        }
        assert!(seen >= 8, "found only {seen} hook_command calls");
    }

    proptest! {
        /// Any path written by `shell_word` reads back as the same path.
        #[test]
        fn a_shell_word_reads_back(
            path in "[^\\x00]{1,40}",
            sub in prop::sample::select(LLMENV_SUBCOMMANDS.to_vec()),
        ) {
            let command = format!("{} {sub} --engine claude_code", shell_word(std::path::Path::new(&path)));
            prop_assert_eq!(classify(&command), Command::Llmenv(PathBuf::from(&path)));
        }

        /// Any text classifies without a panic, and a command that names no llmenv subcommand
        /// never yields a program.
        #[test]
        fn any_command_classifies_and_others_name_no_program(command in "\\PC{0,60}") {
            let class = classify(&command);
            let names_llmenv = command.split_whitespace().any(|w| LLMENV_SUBCOMMANDS.contains(&w))
                || shell_words::split(&command)
                    .is_ok_and(|w| w.iter().any(|w| LLMENV_SUBCOMMANDS.contains(&w.as_str())));
            if !names_llmenv {
                prop_assert_eq!(class, Command::Other);
            }
        }

        /// Other commands mixed in do not change the programs that the llmenv commands name.
        #[test]
        fn other_commands_do_not_change_the_programs(
            others in prop::collection::vec("[a-z]{1,8}( [0-9/.]{1,8}){0,3}", 0..4),
            programs in prop::collection::vec("/[a-z]{1,8}/llmenv", 1..4),
        ) {
            let llmenv: Vec<serde_json::Value> = programs
                .iter()
                .map(|p| serde_json::json!({"command": format!("{p} hook-run --engine claude_code stop")}))
                .collect();
            let mixed: Vec<serde_json::Value> = llmenv
                .iter()
                .cloned()
                .chain(others.iter().map(|c| serde_json::json!({"command": c})))
                .collect();
            let only = rendered_programs(&serde_json::json!({"hooks": {"Stop": [{"hooks": llmenv}]}}));
            let with = rendered_programs(&serde_json::json!({"hooks": {"Stop": [{"hooks": mixed}]}}));
            prop_assert_eq!(only, with);
        }
    }
}
