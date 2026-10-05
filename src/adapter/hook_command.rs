//! Path resolution for a bundle hook command, shared by the settings renderer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    is_bundle_path_char, resolve_bundle_relative_paths, resolve_command_paths_against_files,
};

/// Top-level directories that hold system tools, never bundle scripts.
const SYSTEM_DIRS: [&str; 12] = [
    "dev", "usr", "bin", "sbin", "etc", "tmp", "var", "proc", "sys", "opt", "System", "Library",
];

/// Rewrite the bundle paths in `command` to the cache folder `out`.
///
/// When no path resolves, the command stays as written. A command that names a script outside
/// the bundle files gets one warning for each bundle and path, tracked in `warned`.
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
    let resolved = resolve_bundle_relative_paths(command, out)
        .or_else(|| resolve_command_paths_against_files(command, out, files));
    if resolved.is_none()
        && let Some(token) = unresolved_path_token(command)
    {
        let name = bundle.file_name().map_or_else(
            || bundle.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        if warned.insert(format!("{name}\0{token}")) {
            eprintln!(
                "warning: bundle '{name}': the hook command uses the path `{token}`, which is \
                 not in the bundle files, so it is not moved to the cache folder. The hook may \
                 fail to find its script."
            );
        }
    }
    resolved.unwrap_or_else(|| command.to_string())
}

/// The first word of `command` that names a script path outside the bundle files.
///
/// A word counts when it starts with `/`, `~/`, `$VAR/`, or `${VAR}/`, holds only path
/// characters, and does not point into a system directory. Shell syntax such as the jq `//`
/// operator, `2>/dev/null`, and `$(...)` does not count.
fn unresolved_path_token(command: &str) -> Option<&str> {
    command
        .split_whitespace()
        .map(|word| word.trim_matches(['"', '\'']))
        .find(|word| is_script_path(word))
}

fn is_script_path(word: &str) -> bool {
    let Some(rest) = strip_path_root(word) else {
        return false;
    };
    !word.contains("//")
        && !rest.is_empty()
        && !rest.ends_with('/')
        && rest.chars().all(is_bundle_path_char)
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
            assert_eq!(unresolved_path_token(command), None, "{command}");
        }
    }

    #[test]
    fn rooted_script_paths_are_found() {
        for (command, token) in [
            (
                "bash ${HOME}/git/b/hooks/guard.sh --flag",
                "${HOME}/git/b/hooks/guard.sh",
            ),
            ("bash $HOME/hooks/guard.sh", "$HOME/hooks/guard.sh"),
            ("bash ~/hooks/guard.sh", "~/hooks/guard.sh"),
            ("sh '/home/u/bundle/x.sh'", "/home/u/bundle/x.sh"),
        ] {
            assert_eq!(unresolved_path_token(command), Some(token), "{command}");
        }
    }

    #[test]
    fn a_trailing_slash_or_bare_root_is_not_a_script() {
        for command in ["ls /", "ls $HOME/", "ls ${HOME}", "ls ~/"] {
            assert_eq!(unresolved_path_token(command), None, "{command}");
        }
    }

    #[test]
    fn warning_is_given_once_for_each_bundle_and_path() {
        let files = BTreeMap::new();
        let out = Path::new("/cache");
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
        let got = resolve_hook_command(
            cmd,
            Some(Path::new("/bundles/mine")),
            Path::new("/cache"),
            &BTreeMap::new(),
            &mut warned,
        );
        assert_eq!(got, cmd);
        assert!(warned.is_empty(), "{warned:?}");
    }

    #[test]
    fn a_bundle_relative_path_resolves_into_the_cache() {
        let mut warned = BTreeSet::new();
        let got = resolve_hook_command(
            "bash hooks/guard.sh",
            Some(Path::new("/bundles/mine")),
            Path::new("/cache"),
            &BTreeMap::new(),
            &mut warned,
        );
        assert_eq!(got, "bash /cache/hooks/guard.sh");
        assert!(warned.is_empty());
    }

    #[test]
    fn a_command_without_a_bundle_is_unchanged() {
        let mut warned = BTreeSet::new();
        let got = resolve_hook_command(
            "bash hooks/guard.sh",
            None,
            Path::new("/cache"),
            &BTreeMap::new(),
            &mut warned,
        );
        assert_eq!(got, "bash hooks/guard.sh");
    }
}
