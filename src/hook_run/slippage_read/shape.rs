//! The shapes of a Bash command that count as a single-file view (#2549).

/// The file a command views, when the command is one plain single-file view.
///
/// Accepts `cat FILE`, `head [-n N] FILE`, `tail [-n N] FILE`, and `sed -n N[,M]p FILE`, with
/// `N` and `M` at least 1. Any other shape returns `None`: a pipe, a redirect, a chain, a
/// substitution, a quote, a glob, a `~` path, or a flag this list does not name. The check is on
/// the whole command, because a shell feature can hide a second command or a second file.
///
/// `grep PATTERN FILE` is not accepted. It prints only the matching lines, so it does not show
/// the file. A count of zero also shows no lines, so `head -n 0` is not accepted either.
pub(super) fn single_file_read(command: &str) -> Option<&str> {
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
pub(super) fn is_count(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) && word.bytes().any(|b| b != b'0')
}

/// A `sed` print script such as `10p` or `10,20p`.
pub(super) fn is_sed_range(script: &str) -> bool {
    let Some(range) = script.strip_suffix('p') else {
        return false;
    };
    let bounds: Vec<&str> = range.split(',').collect();
    (1..=2).contains(&bounds.len()) && bounds.iter().all(|b| is_count(b))
}
