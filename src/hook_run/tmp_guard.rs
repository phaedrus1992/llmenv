//! Deny a tool call that writes under `/tmp` (#4045).
//!
//! `TMPDIR` points at `state/tmp`, and `llmenv clean` removes old entries there. Nothing cleans
//! `/tmp`, so a scratch file written there stays for good. The guard reads only the call, so it
//! needs no state.
//!
//! The check reads a path as text. It does not expand a variable and it does not follow a
//! symlink. A command that changes into `/tmp` and then writes a relative path is not seen.

use crate::hook_run::shell_split::{nesting_reason, program, program_words, shell_segments};
use crate::hook_run::shell_tool::{SHELL_TOOLS, is_shell_tool};

/// The tools that write a file by path. Each takes the path in `tool_input`.
const WRITE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// The path roots that nothing cleans. A macOS `/tmp` is a link into `/private/tmp`, and
/// `/var/folders` holds the per-user temp folder of macOS.
const TMP_ROOTS: [&[&str]; 4] = [
    &["tmp"],
    &["private", "tmp"],
    &["var", "folders"],
    &["private", "var", "folders"],
];

/// The commands whose operands are all paths that the command creates or changes.
const WRITE_ALL_OPERANDS: [&str; 4] = ["tee", "touch", "mkdir", "mktemp"];

/// The commands whose last operand, or `-t` value, is the place that the command writes to.
const WRITE_DESTINATION: [&str; 3] = ["cp", "mv", "install"];

/// The tool names that the hook matcher names: the shell tools and the file tools.
pub(crate) fn guarded_tools() -> Vec<&'static str> {
    SHELL_TOOLS.into_iter().chain(WRITE_TOOLS).collect()
}

/// The reason to deny the call, or `None` when the call may run.
///
/// A shell command that cannot be read is denied, so the guard fails closed.
pub(crate) fn deny_reason(payload: &serde_json::Value) -> Option<String> {
    let tool = payload.get("tool_name")?.as_str()?;
    let input = payload.get("tool_input");
    if is_shell_tool(tool) {
        return shell_reason(input);
    }
    if WRITE_TOOLS.contains(&tool) {
        let path = ["file_path", "notebook_path"]
            .iter()
            .find_map(|key| input?.get(key)?.as_str())?;
        return is_tmp_path(path).then(|| reason_for(path));
    }
    None
}

/// The reason to deny a shell call, or `None` when the command writes nowhere under a temp root.
fn shell_reason(input: Option<&serde_json::Value>) -> Option<String> {
    let command = input
        .and_then(|i| i.get("command"))
        .and_then(|c| c.as_str());
    let Some(command) = command else {
        return Some(
            "llmenv blocked this call, because the shell command is missing from the call, so it \
             cannot be checked for a write to /tmp. Send the command as a string."
                .to_string(),
        );
    };
    match shell_write_in_tmp(command) {
        Ok(found) => found.map(|path| reason_for(&path)),
        Err(reason) => Some(reason),
    }
}

/// The message that a deny gives the model: the path, the rule, and the fix.
fn reason_for(found: &str) -> String {
    format!(
        "llmenv blocked this call, because it writes to {found}. \
         Use \"$TMPDIR\" for scratch files. /tmp is not cleaned up."
    )
}

/// The first path under a temp root that the command writes, or an error text when the command
/// cannot be split. `Ok(None)` means the command writes nowhere under a temp root.
fn shell_write_in_tmp(command: &str) -> Result<Option<String>, String> {
    let segments = shell_segments(command).map_err(|_| {
        format!(
            "llmenv blocked this call, because the command cannot be checked for a write to \
             /tmp: {}",
            nesting_reason()
        )
    })?;
    // A `|` or an `&` after `>` ends the segment, so the target is the first word of the next one.
    let nexts = segments
        .iter()
        .skip(1)
        .map(|words| words.first().map(String::as_str));
    Ok(segments
        .iter()
        .zip(nexts.chain(std::iter::once(None)))
        .flat_map(|(words, next)| written_paths(words, next))
        .find(|path| is_tmp_path(path))
        .map(str::to_owned))
}

/// The paths that one command segment writes: redirect targets, and the operands of the commands
/// that write. `next` is the first word of the segment after this one.
fn written_paths<'a>(words: &'a [String], next: Option<&'a str>) -> Vec<&'a str> {
    let mut paths: Vec<&str> = (0..words.len())
        .filter_map(|i| redirect_target(words, i, next))
        .collect();
    let args = program_words(words);
    let Some((first, tail)) = args.split_first() else {
        return paths;
    };
    let name = program(first);
    if WRITE_ALL_OPERANDS.contains(&name) {
        paths.extend(all_operands(name, tail));
    } else if WRITE_DESTINATION.contains(&name) {
        paths.extend(destination(tail));
    }
    paths
}

/// The target of the redirect that starts at `words[i]`: `>x`, `2>x`, `>>x`, `&>x` (the `&` has
/// split the segment already), `>|x`, or a `>` word with the target in the next word.
fn redirect_target<'a>(words: &'a [String], i: usize, next: Option<&'a str>) -> Option<&'a str> {
    let word = words
        .get(i)?
        .trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = word.strip_prefix('>')?;
    let rest = rest.strip_prefix(['>', '|', '&']).unwrap_or(rest);
    if rest.is_empty() {
        return words.get(i + 1).map(String::as_str).or(next);
    }
    Some(rest)
}

/// Every operand of a command that writes all of them. An option with an attached value, such as
/// `--tmpdir=/tmp`, gives the value. `touch -r FILE` reads `FILE`, so that value is skipped.
fn all_operands<'a>(name: &str, args: &[&'a str]) -> Vec<&'a str> {
    let mut operands = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if name == "touch" && matches!(arg, "-r" | "--reference") {
            iter.next();
        } else if let Some((_, value)) = arg.strip_prefix("--").and_then(|a| a.split_once('=')) {
            operands.push(value);
        } else if !arg.starts_with('-') {
            operands.push(arg);
        }
    }
    operands
}

/// The place that `cp`, `mv`, or `install` writes to: the `-t` or `--target-directory` value,
/// else the last operand. The sources are reads, so they are not returned.
fn destination<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut operands = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        match arg {
            "-t" | "--target-directory" => return iter.next(),
            "--" => operands.extend(iter.by_ref()),
            _ => {
                if let Some(value) = arg.strip_prefix("--target-directory=") {
                    return Some(value);
                }
                if !arg.starts_with('-') {
                    operands.push(arg);
                }
            }
        }
    }
    operands.last().copied()
}

/// Whether `path` is under a temp root. The check is on the text of the path, with `.` and `..`
/// resolved, and it ignores the case of a name, because a macOS volume ignores it.
fn is_tmp_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    TMP_ROOTS.iter().any(|root| {
        parts.len() >= root.len()
            && parts
                .iter()
                .zip(root.iter())
                .all(|(part, name)| part.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
