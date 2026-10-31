use super::*;
use proptest::{prop_assert, prop_assert_eq};

proptest::proptest! {
    /// A key with `..` in any segment is never joined into the cache path, whatever the command.
    #[test]
    fn a_traversal_key_is_never_joined_for_any_generated_key(
        prefix in "[a-z]{0,6}",
        suffix in "[a-z]{1,6}",
    ) {
        let key = format!("{prefix}/../{suffix}.sh");
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from(&key), PathBuf::from("/src/tool.sh"));
        let command = format!("bash /root/{key}");
        let got = resolve_command_paths_against_files(&command, Path::new(CACHE), &files);
        prop_assert!(got.is_err(), "a traversal key must fail the hook: {got:?}");
    }

    /// Rewriting never panics on arbitrary command text, and text with no bundle path is unchanged.
    #[test]
    fn rewriting_never_panics_and_keeps_text_without_a_bundle_path(command in "\\PC{0,120}") {
        let files = BTreeMap::new();
        let got = resolve_command_paths_against_files(&command, Path::new(CACHE), &files);
        prop_assert!(
            got.as_ref().is_ok_and(Option::is_none),
            "no known file means no rewrite: {got:?}"
        );
    }

    #[test]
    fn a_root_needs_a_tilde_a_slash_or_a_named_variable(word in "[~$/{}A-Za-z_0-9.]{0,12}") {
        let named = |name: &str| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        let expected = if let Some(rest) = word.strip_prefix("~/") {
            Some(rest)
        } else if let Some(rest) = word.strip_prefix('/') {
            Some(rest)
        } else if let Some(var) = word.strip_prefix("${") {
            var.split_once('}')
                .filter(|(name, _)| !name.is_empty())
                .and_then(|(_, after)| after.strip_prefix('/'))
        } else if let Some(var) = word.strip_prefix('$') {
            var.split_once('/').filter(|(name, _)| named(name)).map(|(_, rest)| rest)
        } else {
            None
        };
        proptest::prop_assert_eq!(strip_path_root(&word), expected);
    }
}

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
fn a_path_that_only_looks_like_it_is_in_the_cache_is_named() {
    assert!(tokens("sh /cache/hooks/x.sh").is_empty());
    assert_eq!(tokens("sh /cache/../etc/x.sh"), ["/cache/../etc/x.sh"]);
    assert_eq!(tokens("sh /cache2/x.sh"), ["/cache2/x.sh"]);
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

#[allow(
    clippy::expect_used,
    reason = "a test helper: a failed resolve must stop the test"
)]
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
    .expect("resolve a hook command in a test")
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
    assert_eq!(
        (first.ok().as_deref(), second.ok().as_deref()),
        (Some(cmd), Some(cmd))
    );
    assert_eq!(warned.len(), 1, "{warned:?}");
    let other = Path::new("/bundles/other");
    let _ = resolve_hook_command(cmd, Some(other), out, &files, &mut warned);
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
#[allow(
    clippy::expect_used,
    reason = "a test: an unexpected error must stop it"
)]
fn a_command_without_a_bundle_is_unchanged() {
    let got = resolve_hook_command(
        "bash hooks/guard.sh",
        None,
        Path::new(CACHE),
        &BTreeMap::new(),
        &mut BTreeSet::new(),
    )
    .expect("a command without a bundle resolves");
    assert_eq!(got, "bash hooks/guard.sh");
}

#[test]
fn a_traversal_key_is_never_joined_into_the_cache() {
    let mut files = BTreeMap::new();
    files.insert(
        PathBuf::from("../escape.sh"),
        PathBuf::from("/src/escape.sh"),
    );
    let got =
        resolve_command_paths_against_files("bash /tmp/../escape.sh", Path::new(CACHE), &files);
    let message = got
        .as_ref()
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        got.is_err() && message.contains("../escape.sh") && message.contains("Fix:"),
        "a traversal key must fail with its key and a fix: {got:?}"
    );
}

// ---- resolve_command_paths_against_files ----

fn known_files_from_paths(paths: &[&str]) -> std::collections::BTreeMap<PathBuf, PathBuf> {
    paths
        .iter()
        .map(|p| (PathBuf::from(p), PathBuf::from(format!("/source/{p}"))))
        .collect()
}

#[allow(
    clippy::expect_used,
    reason = "a test helper: a failed rewrite must stop the test"
)]
fn rewrite(
    command: &str,
    cache: &Path,
    files: &std::collections::BTreeMap<PathBuf, PathBuf>,
) -> Option<String> {
    resolve_command_paths_against_files(command, cache, files).expect("rewrite in a test")
}

#[test]
fn suffix_matches_shell_var_prefixed_token() {
    let files = known_files_from_paths(&["hooks/guard.sh"]);
    let cache = Path::new("/cache");
    let resolved = rewrite("bash ${HOME}/bundles/base/hooks/guard.sh", cache, &files);
    assert_eq!(resolved, Some("bash /cache/hooks/guard.sh".to_string()));
}

#[test]
fn picks_longest_suffix_when_multiple_match() {
    let files = known_files_from_paths(&["guard.sh", "hooks/guard.sh"]);
    let cache = Path::new("/cache");
    let resolved = rewrite("bash ${HOME}/bundles/base/hooks/guard.sh", cache, &files);
    assert_eq!(
        resolved,
        Some("bash /cache/hooks/guard.sh".to_string()),
        "must pick hooks/guard.sh (longer), not guard.sh"
    );
}

#[test]
fn requires_path_component_boundary_before_suffix() {
    // "my-hooks/guard.sh" ends with "hooks/guard.sh" but the substring
    // crosses a component boundary — it should not match.
    let files = known_files_from_paths(&["hooks/guard.sh"]);
    let cache = Path::new("/cache");
    let resolved = rewrite("bash my-hooks/guard.sh", cache, &files);
    assert_eq!(
        resolved, None,
        "must not match suffix that crosses a path-component boundary"
    );
}

#[test]
fn matches_absolute_path_token() {
    // Absolute-path tokens are not blocked — the join operand is the
    // trusted `rel` key, not the untrusted token.
    let files = known_files_from_paths(&["hooks/guard.sh"]);
    let cache = Path::new("/cache");
    let resolved = rewrite("bash /abs/path/hooks/guard.sh", cache, &files);
    assert_eq!(resolved, Some("bash /cache/hooks/guard.sh".to_string()));
}

#[test]
fn empty_known_files_never_matches() {
    let files: std::collections::BTreeMap<PathBuf, PathBuf> = std::collections::BTreeMap::new();
    let cache = Path::new("/cache");
    let resolved = rewrite("bash hooks/guard.sh", cache, &files);
    assert_eq!(resolved, None);
}

#[test]
fn token_without_slash_never_matches() {
    let files = known_files_from_paths(&["guard.sh"]);
    let cache = Path::new("/cache");
    let resolved = rewrite("bash guard.sh", cache, &files);
    assert_eq!(resolved, None);
}

#[test]
fn resolves_multiple_tokens_in_command() {
    let files = known_files_from_paths(&["hooks/pre.sh", "hooks/post.sh"]);
    let cache = Path::new("/cache");
    let resolved = rewrite(
        "bash /some/where/hooks/pre.sh /other/where/hooks/post.sh",
        cache,
        &files,
    );
    assert_eq!(
        resolved,
        Some("bash /cache/hooks/pre.sh /cache/hooks/post.sh".to_string())
    );
}

proptest::proptest! {
    /// The three pieces of a word join back to the word, for any punctuation a command can hold.
    #[test]
    fn split_word_pieces_join_back_to_the_word(word in r#"[a-z/"'();&|$]{0,24}"#) {
        let (prefix, core, suffix) = split_word(&word);
        prop_assert_eq!(format!("{prefix}{core}{suffix}"), word);
    }

    /// The core of a word never keeps an outer quote, bracket, or shell punctuation mark.
    #[test]
    fn split_word_core_has_no_outer_quote_or_punctuation(word in r#"[a-z/"'();&|$]{0,24}"#) {
        let (_, core, _) = split_word(&word);
        prop_assert!(!core.starts_with(['"', '\'', '(']), "core: {core:?}");
        prop_assert!(!core.ends_with(['"', '\'', ';', '&', '|', ')']), "core: {core:?}");
    }

    /// A rewrite that changes no word leaves the command unchanged, and reports no change.
    #[test]
    fn rewrite_words_with_no_change_reports_none(command in r#"[a-z/ \t\n"';&|()$]{0,80}"#) {
        prop_assert!(rewrite_words(&command, |_| None).is_none());
    }

    /// A rewrite that returns each core as it was keeps the command text exactly, spacing included.
    #[test]
    fn rewrite_words_with_identity_keeps_the_text(command in r#"[a-z/ \t\n"';&|()$]{0,80}"#) {
        let got = rewrite_words(&command, |core| Some(core.to_string()));
        prop_assert_eq!(got.unwrap_or_else(|| command.clone()), command.clone());
    }
}
