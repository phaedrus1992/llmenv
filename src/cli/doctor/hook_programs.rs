//! The llmenv programs that the rendered Claude Code hook commands name (#3392).
//!
//! An earlier `llmenv regenerate` can write a path that the running doctor does not share, such
//! as a cargo target binary, so the doctor check reads the written commands, not its own path.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The llmenv subcommands that a built-in hook or the status line runs.
const LLMENV_SUBCOMMANDS: [&str; 6] = [
    "hook-run",
    "config-guard",
    "config-context",
    "check-stale",
    "throttle",
    "statusline",
];

/// The first shell word of `command` and the text after it, in the two forms that
/// `util::shell_word` writes: plain text, or single-quoted parts with `'\''` for a quote.
/// `None` when the word is empty or a quote is not closed.
fn first_word(command: &str) -> Option<(String, &str)> {
    let mut word = String::new();
    let mut rest = command.trim_start();
    loop {
        if let Some(quoted) = rest.strip_prefix('\'') {
            let end = quoted.find('\'')?;
            word.push_str(quoted.get(..end)?);
            rest = quoted.get(end + 1..)?;
        } else if let Some(after) = rest.strip_prefix("\\'") {
            word.push('\'');
            rest = after;
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '\'' || c == '\\')
                .unwrap_or(rest.len());
            if end == 0 {
                break;
            }
            word.push_str(rest.get(..end)?);
            rest = rest.get(end..)?;
        }
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            break;
        }
    }
    (!word.is_empty()).then_some((word, rest))
}

/// The program of `command` when the command runs an llmenv subcommand, else `None`.
fn llmenv_program_of(command: &str) -> Option<PathBuf> {
    let (program, rest) = first_word(command)?;
    let subcommand = rest.split_whitespace().next()?;
    LLMENV_SUBCOMMANDS
        .contains(&subcommand)
        .then(|| PathBuf::from(program))
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

/// The distinct llmenv programs that the commands in `settings` name.
pub(super) fn rendered_programs(settings: &serde_json::Value) -> BTreeSet<PathBuf> {
    commands(settings)
        .into_iter()
        .filter_map(llmenv_program_of)
        .collect()
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
                ]}],
                "Stop": [{"hooks": [{"type": "command", "command": "/opt/bin/llmenv hook-run --engine claude_code stop"}]}],
            },
            "statusLine": {"type": "command", "command": "/tmp/x/llmenv statusline --color always"},
        });
        let programs: Vec<PathBuf> = rendered_programs(&settings).into_iter().collect();
        assert_eq!(
            programs,
            [
                PathBuf::from("/my dir/target/debug/llmenv"),
                PathBuf::from("/opt/bin/llmenv"),
                PathBuf::from("/tmp/x/llmenv"),
            ]
        );
    }

    #[test]
    fn a_settings_file_with_no_hooks_names_no_program() {
        assert!(rendered_programs(&serde_json::json!({})).is_empty());
        assert!(rendered_programs(&serde_json::json!({"hooks": "x"})).is_empty());
    }

    #[test]
    fn an_unclosed_quote_is_not_a_word() {
        assert_eq!(first_word("'/a b hook-run"), None);
        assert_eq!(first_word("   "), None);
    }

    proptest! {
        /// Any path written by `shell_word` reads back as the same path.
        #[test]
        fn a_shell_word_reads_back(path in "[^\\x00\\n]{1,40}", sub in prop::sample::select(LLMENV_SUBCOMMANDS.to_vec())) {
            let command = format!("{} {sub} --engine claude_code", shell_word(std::path::Path::new(&path)));
            prop_assert_eq!(llmenv_program_of(&command), Some(PathBuf::from(&path)));
        }
    }

    #[test]
    fn a_quote_inside_a_path_reads_back() {
        let path = "/a'b/llmenv";
        let command = format!("{} hook-run", shell_word(std::path::Path::new(path)));
        assert_eq!(llmenv_program_of(&command).unwrap(), PathBuf::from(path));
    }
}
