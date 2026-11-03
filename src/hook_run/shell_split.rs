//! Shell command splitting for the task commit gate and the slippage check.
//!
//! The gate and the checks see a command as segments of words. A substitution is split out
//! first, so its command is a segment too. A command that a shell or `eval` runs is split again
//! and added as segments, so the checks never recurse on their own. Split from `task_nudge` so
//! the hook state code stays separate from the parsing (#2627, #2542).

/// The deepest nesting that the splitter follows. The budget counts substitutions, `eval`
/// arguments, and `sh -c` scripts together. Deeper input fails closed, so the hook process stack
/// does not grow with the input (#2544).
pub(super) const MAX_SUBSTITUTION_DEPTH: usize = 32;

/// The command nests deeper than [`MAX_SUBSTITUTION_DEPTH`]. A caller treats the command as unsafe.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct NestingTooDeep;

/// The reason a deny gives for a command nested past [`MAX_SUBSTITUTION_DEPTH`]. It names the
/// limit and the fix, because a retry alone does not explain the refusal.
pub(super) fn nesting_reason() -> String {
    format!(
        "this command nests substitutions more than {MAX_SUBSTITUTION_DEPTH} levels deep, so \
         llmenv cannot check it. Split the command into smaller commands and run each one."
    )
}

/// Move each live `$(...)`, `<(...)`, `>(...)`, and backtick substitution to the start of the
/// segment that holds it, as its own `;`-separated part. Bash runs a substitution before the
/// command that holds it, so the verdict must follow that order (#2544). Single quotes keep
/// their text literal.
#[cfg(test)]
pub(super) fn split_substitutions(command: &str) -> Result<String, NestingTooDeep> {
    split_at_depth(command, 0)
}

/// [`split_substitutions`] for one nesting level. `depth` counts the nesting around `command`.
fn split_at_depth(command: &str, depth: usize) -> Result<String, NestingTooDeep> {
    if depth > MAX_SUBSTITUTION_DEPTH {
        return Err(NestingTooDeep);
    }
    let mut out = String::with_capacity(command.len());
    // The parts of the segment that is being read, and where that segment starts in `out`.
    let mut hoisted = String::new();
    let mut segment_start = 0;
    let mut chars = command.chars().peekable();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        let top = !single && !double;
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            // An escaped character is not a quote, and not a substitution start. This matches
            // `shell_segments`, so both passes agree on quote state.
            '\\' if !single => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                continue;
            }
            '$' if !single && chars.peek() == Some(&'(') => {
                chars.next();
                let inner = split_at_depth(&take_until_close(&mut chars), depth + 1)?;
                hoisted.push_str(&part(&inner));
                continue;
            }
            '`' if !single => {
                let inner: String = chars.by_ref().take_while(|&c| c != '`').collect();
                hoisted.push_str(&part(&split_at_depth(&inner, depth + 1)?));
                continue;
            }
            // Process substitution, `<(...)` and `>(...)`. Inside double quotes it is literal.
            // Bash runs it alongside its command, not before it, so it stays in place (#2544).
            '<' | '>' if !single && !double && chars.peek() == Some(&'(') => {
                chars.next();
                let inner = split_at_depth(&take_until_close(&mut chars), depth + 1)?;
                out.insert_str(segment_start, &hoisted);
                hoisted.clear();
                out.push_str(&part(&inner));
                segment_start = out.len();
                continue;
            }
            ';' | '|' | '&' | '\n' if top => {
                out.insert_str(segment_start, &hoisted);
                hoisted.clear();
                out.push(c);
                segment_start = out.len();
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
    out.insert_str(segment_start, &hoisted);
    Ok(out)
}

/// Whether `command` ends with a substitution and nothing after it, as in `ls && $(cd /x)`. The
/// substitution runs in a subshell, but the split puts its command in the last segment, because
/// the command that holds it is empty (#2937).
///
/// # Errors
/// Returns [`NestingTooDeep`] when the nesting goes past the depth bound.
pub(super) fn ends_with_substitution(command: &str) -> Result<bool, NestingTooDeep> {
    let command = command.trim_end();
    if !command.ends_with([')', '`']) {
        return Ok(false);
    }
    Ok(split_at_depth(command, 0)?.trim_end().ends_with(';'))
}

/// One substitution as a `;`-terminated part. The `;` on each side is a segment boundary for
/// the splitter. The part sits outside any quote, because it goes at a segment start.
fn part(inner: &str) -> String {
    format!(";{inner};")
}

/// Split `command` into segments at the shell operators `;`, `|`, `&`, and a newline, and each
/// segment into words. Quotes group a word and are dropped, so an operator inside quotes does not
/// split. Each `$(...)`, `<(...)`, `>(...)`, and backtick substitution is split out first, so its
/// command is a segment.
///
/// A command that another shell runs is split too, and its segments come before the segment
/// that runs it. These are `eval` arguments and the script of `sh -c`, `bash -c`, and `zsh -c`.
///
/// # Errors
/// Returns [`NestingTooDeep`] when the nesting goes past the depth bound.
pub(super) fn shell_segments(command: &str) -> Result<Vec<Vec<String>>, NestingTooDeep> {
    segments_at(command, 0)
}

/// [`shell_segments`] for one level of nesting. `depth` counts every level around `command`, so
/// substitutions and nested commands share one budget.
fn segments_at(command: &str, depth: usize) -> Result<Vec<Vec<String>>, NestingTooDeep> {
    let command = split_at_depth(command, depth)?;
    let mut segments = Vec::new();
    for words in split_words(&command) {
        if let Some(inner) = nested_command(&words) {
            segments.extend(segments_at(&inner, depth + 1)?);
        }
        segments.push(words);
    }
    Ok(segments)
}

/// The command that a segment hands to another shell: the arguments of `eval`, or the script of
/// `sh -c` and its kin. `None` when the segment runs no nested command.
fn nested_command(words: &[String]) -> Option<String> {
    if let Some(script) = env_split_string(words) {
        return Some(script);
    }
    let words = program_words(words);
    let (first, tail) = words.split_first()?;
    match program(first) {
        "eval" => Some(tail.join(" ")),
        "sh" | "bash" | "zsh" => shell_script(tail).map(str::to_owned),
        _ => None,
    }
}

/// The command string of `env -S` (`--split-string`), which `env` splits into words and runs. The
/// scan reads only the words that `env` takes before its command, so a `-S` in a command's own
/// arguments, such as `git commit -S`, is not read as one.
fn env_split_string(words: &[String]) -> Option<String> {
    let mut rest = words
        .iter()
        .map(|w| w.trim_start_matches(['(', '{']))
        .skip_while(|w| w.contains('=') && !w.starts_with('-'));
    if program(rest.next()?) != "env" {
        return None;
    }
    while let Some(word) = rest.next() {
        match word {
            "-S" | "--split-string" => return rest.next().map(str::to_owned),
            "-u" | "--unset" | "-C" | "--chdir" => {
                rest.next();
            }
            _ => {
                if let Some(value) = word
                    .strip_prefix("-S")
                    .or_else(|| word.strip_prefix("--split-string="))
                {
                    return Some(value.to_owned());
                }
                if !word.starts_with('-') && !word.contains('=') {
                    return None;
                }
            }
        }
    }
    None
}

/// Split `command` into segments of words. The command must already have its substitutions split
/// out, so this pass only groups quotes, escapes, and operators.
fn split_words(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(next) = chars.next() {
                    word.get_or_insert_default().push(next);
                }
            }
            (Some(_), c) => word.get_or_insert_default().push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    word.get_or_insert_default().push(next);
                }
            }
            (None, ';' | '|' | '&' | '\n') => {
                words.extend(word.take());
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            }
            (None, c) if c.is_whitespace() => words.extend(word.take()),
            (None, c) => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word.take());
    if !words.is_empty() {
        segments.push(words);
    }
    segments
}

/// The program name of a word: `/usr/bin/git` is `git`.
pub(super) fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

mod scan;
pub(super) use scan::take_until_close;
mod wrappers;
use wrappers::WRAPPERS;

/// The words of one segment after its group openers, env assignments, and wrappers with their
/// options. The first word is the program. The commit matcher and the task-end matcher both read a
/// segment this way.
pub(super) fn program_words(words: &[String]) -> Vec<&str> {
    let words: Vec<&str> = words
        .iter()
        .map(|w| w.trim_start_matches(['(', '{']))
        .filter(|w| !w.is_empty())
        .collect();
    skip_wrappers(&words).to_vec()
}

/// `rest` without its leading env assignments, and wrappers with the words they take. The first
/// word that is left is the program.
fn skip_wrappers<'a, 'b>(mut rest: &'b [&'a str]) -> &'b [&'a str] {
    while let Some((word, tail)) = rest.split_first() {
        if word.contains('=') && !word.starts_with('-') {
            rest = tail;
        } else if let Some((_, value_opts, operands)) =
            WRAPPERS.iter().find(|(name, _, _)| *name == program(word))
        {
            rest = tail
                .get(wrapper_args(value_opts, *operands, tail)..)
                .unwrap_or_default();
        } else {
            break;
        }
    }
    rest
}

/// The number of words that a wrapper takes after its name: options, the value of a value
/// option, env assignments, and operands. The count stops at the first word that is none of these.
fn wrapper_args(value_opts: &[&str], operands: usize, words: &[&str]) -> usize {
    let mut iter = words.iter();
    let mut used = 0;
    let mut operands = operands;
    while let Some(word) = iter.next() {
        if word.starts_with('-') {
            used += 1;
            if takes_value(word, value_opts) && iter.next().is_some() {
                used += 1;
            }
        } else if word.contains('=') || operands > 0 {
            operands = operands.saturating_sub(1);
            used += 1;
        } else {
            break;
        }
    }
    used
}

/// Whether an option word takes the next word as its value. A short cluster such as `-ks` takes
/// one when its last letter is a value option, as `getopt` reads it. An attached value such as
/// `-sKILL` or `--signal=KILL` does not.
fn takes_value(word: &str, value_opts: &[&str]) -> bool {
    if value_opts.contains(&word) {
        return true;
    }
    match word.strip_prefix('-') {
        Some(cluster) if !cluster.starts_with('-') => cluster
            .chars()
            .last()
            .is_some_and(|last| value_opts.contains(&format!("-{last}").as_str())),
        _ => false,
    }
}

/// Whether one segment starts a commit or a pull request.
/// The first two subcommand words of a `gh` call. A `-R` or `--repo` value is skipped with its flag.
fn gh_subcommands<'a>(tail: &[&'a str]) -> Vec<&'a str> {
    let mut iter = tail.iter();
    let mut subcommands: Vec<&str> = Vec::new();
    while let Some(word) = iter.next() {
        match *word {
            "-R" | "--repo" => {
                iter.next();
            }
            w if w.starts_with('-') => {}
            w => subcommands.push(w),
        }
        if subcommands.len() == 2 {
            break;
        }
    }
    subcommands
}

/// The first subcommand word of a `git` call. A `-C`, `-c`, `--git-dir`, or `--work-tree` value is
/// skipped with its flag.
fn git_subcommand<'a>(tail: &[&'a str]) -> Option<&'a str> {
    let mut iter = tail.iter();
    while let Some(word) = iter.next() {
        match *word {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                iter.next();
            }
            w if w.starts_with('-') => {}
            w => return Some(w),
        }
    }
    None
}

fn starts_commit_or_pr(words: &[String]) -> bool {
    let words = program_words(words);
    let Some((first, tail)) = words.split_first() else {
        return false;
    };
    match program(first) {
        "gh" => gh_subcommands(tail) == ["pr", "create"],
        "git" => git_subcommand(tail) == Some("commit"),
        _ => false,
    }
}

/// The step boundary that one command segment reaches, or `None`. A boundary ends a step of the
/// agent's work: a push, or a pull request opened, marked ready, or merged (#3318).
fn boundary_of(words: &[String]) -> Option<&'static str> {
    let words = program_words(words);
    let (first, tail) = words.split_first()?;
    match program(first) {
        "git" => (git_subcommand(tail) == Some("push")).then_some("git push"),
        "gh" => match gh_subcommands(tail).as_slice() {
            ["pr", "create"] => Some("gh pr create"),
            ["pr", "ready"] => Some("gh pr ready"),
            ["pr", "merge"] => Some("gh pr merge"),
            _ => None,
        },
        _ => None,
    }
}

/// The first step boundary in a shell command, or `None`. A command nested too deeply to split
/// has no boundary that can be named, so the caller is told by `Err`.
pub(super) fn step_boundary(command: &str) -> Result<Option<&'static str>, NestingTooDeep> {
    let segments = shell_segments(command)?;
    Ok(segments.iter().find_map(|words| boundary_of(words)))
}

/// The script that a shell runs with `-c`. The flag may sit in a cluster, as in `bash -lc`.
/// An option that takes a value, such as `-o pipefail`, skips its value. A long option such as
/// `--noclobber` is not a cluster, and a script file has no `-c`.
fn shell_script<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let flags = arg.strip_prefix(['-', '+'])?;
        let short = !flags.starts_with('-');
        if arg.starts_with('-') && short && flags.contains('c') {
            return rest.next().copied();
        }
        let takes_value =
            matches!(*arg, "--rcfile" | "--init-file") || (short && flags.ends_with(['o', 'O']));
        if takes_value {
            rest.next();
        }
    }
    None
}

/// Whether the shell command runs `git commit` or `gh pr create` in any of its parts. A command
/// nested too deeply to split counts as one, so the gate fails closed.
pub(super) fn runs_commit_or_pr(command: &str) -> bool {
    match shell_segments(command) {
        Ok(segments) => segments.iter().any(|words| starts_commit_or_pr(words)),
        Err(NestingTooDeep) => true,
    }
}

/// Whether `llmenv task done <wip>` or `llmenv task wait <wip>` runs before the first commit or
/// pull request segment. Another task's end does not end the task `wip`. A command nested too
/// deeply to split may end the task, so it counts as ending it and the gate denies the commit.
pub(super) fn ends_task_before_commit(command: &str, wip: &str) -> bool {
    let Ok(segments) = shell_segments(command) else {
        return true;
    };
    for words in segments {
        if starts_commit_or_pr(&words) {
            return false;
        }
        if task_end_target(&words) == Some(wip) {
            return true;
        }
    }
    false
}

/// The task that a `llmenv task done` or `llmenv task wait` segment names, or `None`.
fn task_end_target(words: &[String]) -> Option<&str> {
    let words = program_words(words);
    match words.as_slice() {
        [first, "task", "done" | "wait", target, ..] if program(first) == "llmenv" => Some(*target),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
