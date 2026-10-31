//! Path resolution for a bundle hook command, shared by every engine adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Top-level directories that hold system tools, never bundle scripts.
const SYSTEM_DIRS: [&str; 12] = [
    "dev", "usr", "bin", "sbin", "etc", "tmp", "var", "proc", "sys", "opt", "System", "Library",
];

/// A character that can appear in a script path. A word with any other character is shell
/// syntax (`2>/dev/null`, `$(...)`) and is never a path.
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._-+@/".contains(c)
}

/// Split `word` into the quote or bracket before it, the word itself, and the quote or shell
/// punctuation after it, so `"hooks/guard.sh";` still reads as a path.
fn split_word(word: &str) -> (&str, &str, &str) {
    let rest = word.trim_start_matches(['"', '\'', '(']);
    let core = rest.trim_end_matches(['"', '\'', ';', '&', '|', ')']);
    let prefix = &word[..word.len() - rest.len()];
    (prefix, core, &rest[core.len()..])
}

/// Rewrite each word of `command` with `rewrite`, which sees the word without its quotes and
/// trailing punctuation. Spacing and newlines stay as written. Returns `None` when no word
/// changed.
fn rewrite_words(command: &str, mut rewrite: impl FnMut(&str) -> Option<String>) -> Option<String> {
    let mut changed = false;
    let mut result = String::with_capacity(command.len());
    for chunk in command.split_inclusive(char::is_whitespace) {
        let word = chunk.trim_end_matches(char::is_whitespace);
        let (prefix, core, suffix) = split_word(word);
        match rewrite(core).filter(|_| !core.is_empty()) {
            Some(new) => {
                changed = true;
                result.push_str(&format!("{prefix}{new}{suffix}"));
            }
            None => result.push_str(word),
        }
        result.push_str(&chunk[word.len()..]);
    }
    changed.then_some(result)
}

/// Resolve bundle-relative paths in a hook command string.
///
/// A word that holds `/`, does not start with `/`, `~`, `$`, or `-`, and has path characters
/// only becomes an absolute path under `bundle_dir`. A quote or `;` around the word stays.
///
/// Shared across adapters: any engine that renders a hook `command` string must resolve
/// bundle-relative script paths the same way, since a bundle is authored once and materialized
/// for every engine.
pub(crate) fn resolve_bundle_relative_paths(command: &str, bundle_dir: &Path) -> Option<String> {
    rewrite_words(command, |word| {
        let is_relative = word.contains('/')
            && !word.starts_with(['/', '~', '$', '-'])
            && word.chars().all(is_path_char)
            && !crate::paths::is_unsafe_join_target(word);
        is_relative.then(|| bundle_dir.join(word).to_string_lossy().into_owned())
    })
}

/// Rewrite bundle-authored hook commands that reference files copied into the cache directory,
/// even when the command uses shell variables or absolute paths that
/// [`resolve_bundle_relative_paths`] cannot match.
///
/// A word that holds `/` and **ends with** a relative path in `known_files`, at a path-component
/// boundary, has that suffix replaced with `cache_dir.join(rel)`. The **longest** matching suffix
/// wins. A word that matches no known file stays as written.
///
/// ```text
/// bash ${HOME}/git/my-llmenv/bundles/base/hooks/guard.sh
/// ```
///
/// The word above ends with `hooks/guard.sh`, a file that was copied into the cache.
fn resolve_command_paths_against_files(
    command: &str,
    cache_dir: &Path,
    known_files: &BTreeMap<PathBuf, PathBuf>,
) -> anyhow::Result<Option<String>> {
    // Longest key first, so the first match is the most specific suffix.
    let mut candidates: Vec<(&Path, String)> = known_files
        .keys()
        .map(|k| (k.as_path(), k.to_string_lossy().into_owned()))
        .collect();
    candidates.sort_by_key(|(_, s)| std::cmp::Reverse(s.len()));

    // A traversal key that matches a word fails the hook (#3120). It is never joined.
    let mut refused: Option<&str> = None;
    let rewritten = rewrite_words(command, |word| {
        if !word.contains('/') {
            return None;
        }
        let (rel, key) = candidates
            .iter()
            .find(|(_, s)| suffix_matches(word, s.as_str()))?;
        if crate::paths::is_unsafe_join_target(key) {
            refused.get_or_insert(key.as_str());
            return None;
        }
        Some(cache_dir.join(rel).to_string_lossy().into_owned())
    });
    match refused {
        Some(key) => Err(anyhow::anyhow!(
            "bundle file `{key}` has a path that leaves the bundle, so the hook command cannot \
             use it. Fix: rename the bundle file so its path stays inside the bundle, then \
             regenerate."
        )),
        None => Ok(rewritten),
    }
}

/// True when `word` ends with `key` at a path-component boundary.
fn suffix_matches(word: &str, key: &str) -> bool {
    let prefix_len = word.len().saturating_sub(key.len());
    word.ends_with(key) && (prefix_len == 0 || word.as_bytes().get(prefix_len - 1) == Some(&b'/'))
}

/// Rewrite the bundle paths in `command` to the cache folder `out`.
///
/// A command with no bundle path stays as written. A rooted script path that is not in the
/// bundle files gets one warning for each bundle and path, tracked in `warned`.
pub(crate) fn resolve_hook_command(
    command: &str,
    bundle_origin: Option<&Path>,
    out: &Path,
    files: &BTreeMap<PathBuf, PathBuf>,
    warned: &mut BTreeSet<String>,
) -> anyhow::Result<String> {
    let Some(bundle) = bundle_origin else {
        return Ok(command.to_string());
    };
    let name = bundle.file_name().map_or_else(
        || bundle.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let relative = resolve_bundle_relative_paths(command, out);
    let by_suffix =
        resolve_command_paths_against_files(relative.as_deref().unwrap_or(command), out, files)
            .with_context(|| format!("bundle '{name}': cannot resolve the hook command"))?;
    let resolved = by_suffix
        .or(relative)
        .unwrap_or_else(|| command.to_string());
    for token in unresolved_path_tokens(&resolved, out) {
        if warned.insert(format!("{name}\0{token}")) {
            eprintln!(
                "warning: bundle '{name}': the hook command uses the path `{token}`, which is \
                 not in the bundle files, so it is not moved to the cache folder. Add the file \
                 to the bundle, or use a path inside the bundle. The hook may fail to find its \
                 script."
            );
        }
    }
    Ok(resolved)
}

/// The words of `command` that name a script path outside the bundle files and outside `cache`.
///
/// A word counts when it starts with `/`, `~/`, `$VAR/`, or `${VAR}/`, holds only path
/// characters, and does not point into a system directory. Shell syntax such as the jq `//`
/// operator, `2>/dev/null`, and `$(...)` does not count.
fn unresolved_path_tokens(command: &str, cache: &Path) -> Vec<String> {
    command
        .split_whitespace()
        .map(|word| split_word(word).1)
        // Components, not text: `/cache/../etc/x` and `/cache2/x` are outside `/cache` (#2923).
        .filter(|word| is_script_path(word) && !crate::util::is_within(cache, word))
        .map(str::to_string)
        .collect()
}

fn is_script_path(word: &str) -> bool {
    let Some(rest) = strip_path_root(word) else {
        return false;
    };
    !word.contains("//")
        && !rest.is_empty()
        && !rest.ends_with('/')
        && rest.chars().all(is_path_char)
        && !(word.starts_with('/') && is_system_path(rest))
}

/// The part of `word` after its root (`/`, `~/`, `$VAR/`, or `${VAR}/`), or `None`.
fn strip_path_root(word: &str) -> Option<&str> {
    if let Some(rest) = word.strip_prefix("~/").or_else(|| word.strip_prefix('/')) {
        return Some(rest);
    }
    let var = word.strip_prefix('$')?;
    let (name, after_var) = if let Some(braced) = var.strip_prefix('{') {
        braced.split_once('}')?
    } else {
        let after = var.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
        (&var[..var.len() - after.len()], after)
    };
    // `$/x` names no variable, so it is not a rooted path.
    if name.is_empty() {
        return None;
    }
    after_var.strip_prefix('/')
}

fn is_system_path(rest: &str) -> bool {
    let top = rest.split('/').next().unwrap_or_default();
    SYSTEM_DIRS.contains(&top)
}

#[cfg(test)]
mod tests;
