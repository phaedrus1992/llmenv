//! Record a file as read when a Bash command views it (#2549).
//!
//! The read-before-Write gate in `slippage` counts a `Read` tool call. Claude Code 2.1.293 also
//! treats a single-file `cat`, `head`, `tail`, `sed -n`, or `grep` as a file view, and auto mode
//! tells the agent to use them. Without this module those views would not count, and a Write
//! after a Bash read would be denied.

use std::path::Path;

use serde_json::Value;

use crate::hook_run::slippage::{load_stats, path_key, save_stats};

/// Record the file a Bash `PostToolUse` read, if the command is one plain single-file view.
///
/// A relative path resolves against the payload `cwd`. Without a `cwd`, a relative path is
/// not recorded. A view that is not recorded leaves the file unread, so a later Write to it
/// is denied. The deny is conservative, and its text says the file was not read.
pub(super) fn record_bash_read(state_dir: &Path, session_id: &str, payload: &Value) {
    let Some(command) = payload
        .get("tool_input")
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(file) = single_file_read(command) else {
        return;
    };
    // `Path::join` returns the right side unchanged when it is absolute, so one join covers both.
    let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
        if Path::new(file).is_absolute() {
            record_path(state_dir, session_id, Path::new(file));
        }
        return;
    };
    record_path(state_dir, session_id, &Path::new(cwd).join(file));
}

fn record_path(state_dir: &Path, session_id: &str, path: &Path) {
    let mut stats = load_stats(state_dir, session_id);
    stats.paths.insert(path_key(&path.to_string_lossy()));
    save_stats(state_dir, session_id, &stats);
}

/// The file a command views, when the command is one plain single-file view.
///
/// Accepts `cat FILE`, `head [-n N] FILE`, `tail [-n N] FILE`, and `sed -n N[,M]p FILE`, with
/// `N` and `M` at least 1. Any other shape returns `None`: a pipe, a redirect, a chain, a
/// substitution, a quote, a glob, a `~` path, or a flag this list does not name. The check is on
/// the whole command, because a shell feature can hide a second command or a second file.
///
/// `grep PATTERN FILE` is not accepted. It prints only the matching lines, so it does not show
/// the file. A count of zero also shows no lines, so `head -n 0` is not accepted either.
fn single_file_read(command: &str) -> Option<&str> {
    const SHELL_SYNTAX: &[char] = &[
        '|', '>', '<', '&', ';', '`', '$', '\n', '\'', '"', '\\', '*', '?', '(', ')',
    ];
    if command.contains(SHELL_SYNTAX) {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    match words.as_slice() {
        ["cat", file] | ["head" | "tail", file] => plain_file(file),
        ["head" | "tail", "-n", count, file] if is_count(count) => plain_file(file),
        ["sed", "-n", script, file] if is_sed_range(script) => plain_file(file),
        _ => None,
    }
}

/// A file argument. A word that starts with `-` is a flag this module does not understand. A
/// word that starts with `~` is a path the shell expands, and this module does not expand it.
fn plain_file(word: &str) -> Option<&str> {
    (!word.starts_with(['-', '~'])).then_some(word)
}

/// A positive line count or line number: ASCII digits that are not all zero.
fn is_count(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) && word.bytes().any(|b| b != b'0')
}

/// A `sed` print script such as `10p` or `10,20p`.
fn is_sed_range(script: &str) -> bool {
    let Some(range) = script.strip_suffix('p') else {
        return false;
    };
    let bounds: Vec<&str> = range.split(',').collect();
    (1..=2).contains(&bounds.len()) && bounds.iter().all(|b| is_count(b))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn a_count_is_a_positive_digit_run(word in "[0-9a-z+,p-]{0,6}") {
            let expected = !word.is_empty()
                && word.bytes().all(|b| b.is_ascii_digit())
                && word.bytes().any(|b| b != b'0');
            prop_assert_eq!(is_count(&word), expected);
        }

        #[test]
        fn a_sed_print_range_takes_one_or_two_positive_bounds(n in 1_u32..100_000, m in 1_u32..100_000) {
            let one = format!("{}p", n);
            let two = format!("{},{}p", n, m);
            let three = format!("{},{},{}p", n, m, n);
            let zero = format!("0,{}p", m);
            prop_assert!(is_sed_range(&one));
            prop_assert!(is_sed_range(&two));
            prop_assert!(!is_sed_range(&three));
            prop_assert!(!is_sed_range(&zero));
            prop_assert!(!is_sed_range(&n.to_string()));
        }

        #[test]
        fn a_shell_metacharacter_is_never_a_read(
            prefix in "[a-z ]{0,10}",
            meta in prop::sample::select(vec![
                '|', '>', '<', '&', ';', '`', '$', '\'', '"', '\\', '*', '?', '(', ')',
            ]),
            tail in "[a-z ]{0,10}",
        ) {
            let command = format!("cat {prefix}{meta}{tail}");
            prop_assert_eq!(single_file_read(&command), None);
        }

        #[test]
        fn a_returned_file_is_one_plain_word_of_the_command(command in "[a-z0-9 ./~-]{0,30}") {
            if let Some(file) = single_file_read(&command) {
                prop_assert!(command.split_whitespace().any(|w| w == file));
                prop_assert!(!file.is_empty());
                prop_assert!(!file.starts_with(['-', '~']));
            }
        }
    }

    #[test]
    fn accepts_each_single_file_view_shape() {
        for (command, file) in [
            ("cat notes.md", "notes.md"),
            ("head src/lib.rs", "src/lib.rs"),
            ("tail -n 20 /var/log/x", "/var/log/x"),
            ("sed -n 10,20p src/main.rs", "src/main.rs"),
            ("sed -n 7p src/main.rs", "src/main.rs"),
        ] {
            assert_eq!(single_file_read(command), Some(file), "{command}");
        }
    }

    #[test]
    fn rejects_shapes_that_are_not_one_plain_view() {
        for command in [
            "cat a.md b.md",
            "cat",
            "cat -n a.md",
            "cat a.md | wc -l",
            "cat a.md > out.txt",
            "cat < a.md",
            "cat a.md && rm a.md",
            "cat a.md; rm a.md",
            "cat $(echo a.md)",
            "cat `echo a.md`",
            "cat \"a.md\"",
            "cat 'a.md'",
            "cat *.md",
            "cat a.md\nrm a.md",
            "head -c 5 a.md",
            "head -n five a.md",
            "sed -i 10p a.md",
            "sed -n 10,20,30p a.md",
            "sed -n 10 a.md",
            "sed -n 0p a.md",
            "head -n 0 a.md",
            "tail -n 00 a.md",
            "cat ~/notes.md",
            "head ~/notes.md",
            "grep TODO src/lib.rs",
            "grep -r TODO src",
            "rg TODO src/lib.rs",
            "less a.md",
            "",
        ] {
            assert_eq!(single_file_read(command), None, "{command:?}");
        }
    }

    #[test]
    fn a_relative_read_resolves_against_the_payload_cwd() {
        let state = tempfile::tempdir().expect("state dir");
        let work = tempfile::tempdir().expect("work dir");
        let file = work.path().join("note.md");
        std::fs::write(&file, "keep").expect("write file");
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cat note.md" },
            "cwd": work.path(),
        });

        record_bash_read(state.path(), "s1", &payload);

        let stats = load_stats(state.path(), "s1");
        assert!(stats.paths.contains(&path_key(&file.to_string_lossy())));
    }

    #[test]
    fn a_relative_read_without_a_cwd_is_not_recorded() {
        let state = tempfile::tempdir().expect("state dir");
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cat note.md" },
        });

        record_bash_read(state.path(), "s1", &payload);

        assert!(load_stats(state.path(), "s1").paths.is_empty());
    }

    fn read_before_edit_on() -> crate::config::SlippageControl {
        crate::config::SlippageControl {
            enabled: true,
            read_before_edit: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_bash_view_lets_the_write_through() {
        let state = tempfile::tempdir().expect("state dir");
        let work = tempfile::tempdir().expect("work dir");
        let file = work.path().join("existing.txt");
        std::fs::write(&file, "important").expect("write file");
        let cfg = read_before_edit_on();
        let view = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cat existing.txt" },
            "cwd": work.path(),
        });
        let write = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "file_path": file },
        });

        crate::hook_run::slippage::handle_post_tool_use(
            Some(&cfg),
            &view,
            Some("s1"),
            state.path(),
        );

        assert_eq!(
            crate::hook_run::slippage::handle_pre_tool_use(
                Some(&cfg),
                &write,
                Some("s1"),
                state.path()
            ),
            ""
        );
    }

    #[test]
    fn a_bash_view_is_ignored_when_the_gate_is_off() {
        let state = tempfile::tempdir().expect("state dir");
        let work = tempfile::tempdir().expect("work dir");
        let file = work.path().join("existing.txt");
        std::fs::write(&file, "important").expect("write file");
        let cfg = crate::config::SlippageControl {
            enabled: true,
            read_before_edit: false,
            metrics: true,
            ..Default::default()
        };
        let view = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cat existing.txt" },
            "cwd": work.path(),
        });
        let write = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "file_path": file },
        });

        crate::hook_run::slippage::handle_post_tool_use(
            Some(&cfg),
            &view,
            Some("s1"),
            state.path(),
        );
        let gate = crate::hook_run::slippage::handle_pre_tool_use(
            Some(&read_before_edit_on()),
            &write,
            Some("s1"),
            state.path(),
        );

        assert!(gate.starts_with("__DENY__:"), "{gate}");
    }

    #[test]
    fn a_missing_read_log_is_not_reported() {
        let state = tempfile::tempdir().expect("state dir");
        let logs = crate::test_log_capture::capture_logs(|| {
            assert!(load_stats(state.path(), "s1").paths.is_empty());
        });
        assert!(!logs.contains("read log"), "{logs}");
    }

    #[test]
    fn a_corrupt_read_log_is_reported_and_read_as_empty() {
        let state = tempfile::tempdir().expect("state dir");
        let path = crate::hook_run::slippage::stats_path(state.path(), "s1");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"{not json").expect("write log");
        let logs = crate::test_log_capture::capture_logs(|| {
            assert!(load_stats(state.path(), "s1").paths.is_empty());
        });
        assert!(logs.contains("read log is corrupt"), "{logs}");
    }

    #[test]
    fn an_unreadable_read_log_is_reported_and_read_as_empty() {
        let state = tempfile::tempdir().expect("state dir");
        // A directory at the log path fails to read with an error other than NotFound.
        let path = crate::hook_run::slippage::stats_path(state.path(), "s1");
        std::fs::create_dir_all(&path).expect("mkdir as the log path");
        let logs = crate::test_log_capture::capture_logs(|| {
            assert!(load_stats(state.path(), "s1").paths.is_empty());
        });
        assert!(logs.contains("read log unreadable"), "{logs}");
    }

    #[test]
    fn a_piped_read_is_not_recorded() {
        let state = tempfile::tempdir().expect("state dir");
        let work = tempfile::tempdir().expect("work dir");
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cat note.md | wc -l" },
            "cwd": work.path(),
        });

        record_bash_read(state.path(), "s1", &payload);

        assert!(load_stats(state.path(), "s1").paths.is_empty());
    }
}
