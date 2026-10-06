//! Path resolution for a bundle hook command, shared by every engine adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
pub(crate) fn resolve_command_paths_against_files(
    command: &str,
    cache_dir: &Path,
    known_files: &BTreeMap<PathBuf, PathBuf>,
) -> Option<String> {
    // Longest key first, so the first match is the most specific suffix.
    let mut candidates: Vec<(&Path, String)> = known_files
        .keys()
        .map(|k| (k.as_path(), k.to_string_lossy().into_owned()))
        .collect();
    candidates.sort_by_key(|(_, s)| std::cmp::Reverse(s.len()));

    rewrite_words(command, |word| {
        if !word.contains('/') {
            return None;
        }
        // The join operand is `rel`, a trusted key from known_files, so an absolute or `../`
        // word is safe to suffix-match.
        let (rel, _) = candidates.iter().find(|(_, s)| {
            let prefix_len = word.len().saturating_sub(s.len());
            word.ends_with(s.as_str())
                && (prefix_len == 0 || word.as_bytes().get(prefix_len - 1) == Some(&b'/'))
        })?;
        debug_assert!(
            !crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()),
            "known_files key contains traversal: {}",
            rel.display()
        );
        Some(cache_dir.join(rel).to_string_lossy().into_owned())
    })
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
) -> String {
    let Some(bundle) = bundle_origin else {
        return command.to_string();
    };
    let relative = resolve_bundle_relative_paths(command, out);
    let by_suffix =
        resolve_command_paths_against_files(relative.as_deref().unwrap_or(command), out, files);
    let resolved = by_suffix
        .or(relative)
        .unwrap_or_else(|| command.to_string());
    let name = bundle.file_name().map_or_else(
        || bundle.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
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
    resolved
}

/// The words of `command` that name a script path outside the bundle files and outside `cache`.
///
/// A word counts when it starts with `/`, `~/`, `$VAR/`, or `${VAR}/`, holds only path
/// characters, and does not point into a system directory. Shell syntax such as the jq `//`
/// operator, `2>/dev/null`, and `$(...)` does not count.
fn unresolved_path_tokens(command: &str, cache: &Path) -> Vec<String> {
    let cache = cache.to_string_lossy();
    command
        .split_whitespace()
        .map(|word| split_word(word).1)
        .filter(|word| is_script_path(word) && !word.starts_with(cache.as_ref()))
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
    let after_var = if let Some(braced) = var.strip_prefix('{') {
        braced.split_once('}')?.1
    } else {
        var.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_')
    };
    after_var.strip_prefix('/')
}

fn is_system_path(rest: &str) -> bool {
    let top = rest.split('/').next().unwrap_or_default();
    SYSTEM_DIRS.contains(&top)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CACHE: &str = "/cache";

    fn tokens(command: &str) -> Vec<String> {
        unresolved_path_tokens(command, Path::new(CACHE))
    }

    #[test]
    fn inline_shell_commands_name_no_script_path() {
        for command in [
            r#"f=$(jq -r '.tool_input.file_path // empty'); case "$f" in *.rs) cd "$(dirname "$f")" && cargo clippy --quiet 2>&1 | head -20;; esac"#,
            "cargo test 2>/dev/null",
            "/usr/bin/env bash -c true",
            "echo $((4/2))",
            "curl https://example.com/hook",
            "ls /tmp/x",
            "echo a/b",
        ] {
            assert!(tokens(command).is_empty(), "{command}");
        }
    }

    #[test]
    fn rooted_script_paths_are_found_with_quotes_and_punctuation_removed() {
        for (command, token) in [
            (
                "bash ${HOME}/git/b/hooks/guard.sh --flag",
                "${HOME}/git/b/hooks/guard.sh",
            ),
            ("bash $HOME/hooks/guard.sh", "$HOME/hooks/guard.sh"),
            ("bash ~/hooks/guard.sh", "~/hooks/guard.sh"),
            ("sh '/home/u/bundle/x.sh'", "/home/u/bundle/x.sh"),
            ("sh /home/u/x.sh;", "/home/u/x.sh"),
            ("(sh $HOME/x.sh)", "$HOME/x.sh"),
        ] {
            assert_eq!(tokens(command), [token], "{command}");
        }
    }

    #[test]
    fn every_unresolved_path_is_named() {
        assert_eq!(
            tokens("sh /a/one.sh && sh /b/two.sh"),
            ["/a/one.sh", "/b/two.sh"]
        );
    }

    #[test]
    fn a_path_under_the_cache_folder_is_resolved() {
        assert!(tokens("bash /cache/hooks/guard.sh").is_empty());
    }

    #[test]
    fn a_trailing_slash_or_bare_root_is_not_a_script() {
        for command in ["ls /", "ls $HOME/", "ls ${HOME}", "ls ~/"] {
            assert!(tokens(command).is_empty(), "{command}");
        }
    }

    fn resolve(command: &str, files: &[&str], warned: &mut BTreeSet<String>) -> String {
        let files = files
            .iter()
            .map(|f| (PathBuf::from(f), PathBuf::from(format!("/src/{f}"))))
            .collect();
        resolve_hook_command(
            command,
            Some(Path::new("/bundles/mine")),
            Path::new(CACHE),
            &files,
            warned,
        )
    }

    #[test]
    fn warning_is_given_once_for_each_bundle_and_path() {
        let files = BTreeMap::new();
        let out = Path::new(CACHE);
        let bundle = Path::new("/bundles/mine");
        let mut warned = BTreeSet::new();
        let cmd = "bash $HOME/x/guard.sh";
        let first = resolve_hook_command(cmd, Some(bundle), out, &files, &mut warned);
        let second = resolve_hook_command(cmd, Some(bundle), out, &files, &mut warned);
        assert_eq!((first.as_str(), second.as_str()), (cmd, cmd));
        assert_eq!(warned.len(), 1, "{warned:?}");
        let other = Path::new("/bundles/other");
        resolve_hook_command(cmd, Some(other), out, &files, &mut warned);
        assert_eq!(warned.len(), 2, "{warned:?}");
    }

    #[test]
    fn inline_command_leaves_the_warning_set_empty() {
        let mut warned = BTreeSet::new();
        let cmd = "jq -r '.a // empty' 2>/dev/null";
        assert_eq!(resolve(cmd, &[], &mut warned), cmd);
        assert!(warned.is_empty(), "{warned:?}");
    }

    #[test]
    fn a_bundle_relative_path_resolves_into_the_cache() {
        let mut warned = BTreeSet::new();
        let got = resolve("bash hooks/guard.sh", &[], &mut warned);
        assert_eq!(got, "bash /cache/hooks/guard.sh");
        assert!(warned.is_empty());
    }

    #[test]
    fn a_quoted_or_terminated_relative_path_resolves_with_its_punctuation() {
        let dir = Path::new("/b");
        for (command, want) in [
            ("bash 'hooks/g.sh'", "bash '/b/hooks/g.sh'"),
            (r#"bash "hooks/g.sh""#, r#"bash "/b/hooks/g.sh""#),
            ("bash hooks/g.sh; echo", "bash /b/hooks/g.sh; echo"),
            ("(bash hooks/g.sh)", "(bash /b/hooks/g.sh)"),
        ] {
            assert_eq!(
                resolve_bundle_relative_paths(command, dir).as_deref(),
                Some(want),
                "{command}"
            );
        }
    }

    #[test]
    fn spacing_and_newlines_survive_a_rewrite() {
        let command = "set -e\nbash  hooks/g.sh\n\techo done\n";
        assert_eq!(
            resolve_bundle_relative_paths(command, Path::new("/b")).as_deref(),
            Some("set -e\nbash  /b/hooks/g.sh\n\techo done\n")
        );
    }

    #[test]
    fn a_quoted_rooted_path_resolves_by_suffix() {
        let mut warned = BTreeSet::new();
        let got = resolve(
            r#"bash "${HOME}/git/b/hooks/guard.sh""#,
            &["hooks/guard.sh"],
            &mut warned,
        );
        assert_eq!(got, r#"bash "/cache/hooks/guard.sh""#);
        assert!(warned.is_empty(), "{warned:?}");
    }

    #[test]
    fn a_rooted_path_is_resolved_beside_a_relative_one() {
        let mut warned = BTreeSet::new();
        let got = resolve(
            "bash hooks/a.sh ${HOME}/b/hooks/c.sh /opt/x/d.sh",
            &["hooks/c.sh"],
            &mut warned,
        );
        assert_eq!(got, "bash /cache/hooks/a.sh /cache/hooks/c.sh /opt/x/d.sh");
        assert!(warned.is_empty(), "/opt is a system folder: {warned:?}");
    }

    #[test]
    fn a_suffix_match_needs_a_path_component_boundary() {
        let mut warned = BTreeSet::new();
        let cmd = "bash /x/myhooks/guard.sh";
        assert_eq!(resolve(cmd, &["hooks/guard.sh"], &mut warned), cmd);
        assert_eq!(warned.len(), 1, "the path stays unresolved: {warned:?}");
    }

    #[test]
    fn an_unresolved_rooted_path_warns_beside_a_resolved_one() {
        let mut warned = BTreeSet::new();
        resolve("bash hooks/a.sh /home/u/b/other.sh", &[], &mut warned);
        assert_eq!(warned.len(), 1, "{warned:?}");
    }

    #[test]
    fn a_command_without_a_bundle_is_unchanged() {
        let got = resolve_hook_command(
            "bash hooks/guard.sh",
            None,
            Path::new(CACHE),
            &BTreeMap::new(),
            &mut BTreeSet::new(),
        );
        assert_eq!(got, "bash hooks/guard.sh");
    }
}
