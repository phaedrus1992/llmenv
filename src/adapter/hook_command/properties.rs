#![expect(clippy::unwrap_used, reason = "test code: an error must stop the test")]

use super::*;
use proptest::prelude::*;

use super::tests::{CACHE, tokens};

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

    /// `p/k` resolves to the longest key `k`, not to a shorter key that is also its suffix, and
    /// `p` + a non-`/` character + `k` does not resolve (#3122).
    #[test]
    fn the_longest_key_wins_and_a_match_needs_a_component_boundary(
        root in "/[a-z]{1,5}",
        dirs in proptest::collection::vec("[a-z]{1,5}", 1..3),
        file in "[a-z]{1,5}\\.sh",
        glue in "[a-z.\\-_]",
    ) {
        let key = format!("{}/{file}", dirs.join("/"));
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from(&key), PathBuf::from("/src/long.sh"));
        files.insert(PathBuf::from(&file), PathBuf::from("/src/short.sh"));
        let cache = Path::new(CACHE);
        let got = resolve_command_paths_against_files(&format!("bash {root}/{key}"), cache, &files);
        let want = format!("bash {}", cache.join(&key).display());
        prop_assert_eq!(got.ok().flatten(), Some(want));

        let only_long: BTreeMap<_, _> = files.into_iter().filter(|(k, _)| *k == Path::new(&key)).collect();
        let joined = format!("bash {root}{glue}{key}");
        let got = resolve_command_paths_against_files(&joined, cache, &only_long);
        prop_assert_eq!(got.ok().flatten(), None);
    }

    /// A command that already points into the cache does not change on a second pass.
    #[test]
    fn a_second_rewrite_changes_nothing(
        root in "/[a-z]{1,5}",
        dirs in proptest::collection::vec("[a-z]{1,5}", 0..3),
        file in "[a-z]{1,5}\\.sh",
        tail in "( [a-z]{1,5}){0,2}",
    ) {
        let key = dirs.iter().map(String::as_str).chain([file.as_str()]).collect::<Vec<_>>().join("/");
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from(&key), PathBuf::from("/src/tool.sh"));
        let cache = Path::new(CACHE);
        let command = format!("bash {root}/{key}{tail}");
        let once = resolve_command_paths_against_files(&command, cache, &files)
            .unwrap()
            .unwrap_or(command);
        let twice = resolve_command_paths_against_files(&once, cache, &files)
            .unwrap()
            .unwrap_or_else(|| once.clone());
        prop_assert_eq!(twice, once);
    }

    /// Each token named as unresolved is a script path, is outside the cache, and holds no `//`.
    #[test]
    fn every_unresolved_token_is_a_script_path_outside_the_cache(
        words in proptest::collection::vec(prop_oneof![
            "/[a-z/.]{0,12}",
            "~/[a-z/.]{0,8}",
            "\\$[A-Z]{0,3}/[a-z/]{0,8}",
            "/cache/[a-z./]{0,8}",
            "[a-z|>&;'\"]{0,6}",
            "\\PC{0,8}",
        ], 0..6)
    ) {
        for token in tokens(&words.join(" ")) {
            prop_assert!(is_script_path(&token), "{token}");
            prop_assert!(!crate::util::is_within(Path::new(CACHE), &token), "{token}");
            prop_assert!(!token.contains("//"), "{token}");
        }
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

proptest::proptest! {
    /// `suffix_matches` is "equal to the key, or ends with `/` + key", for any text including
    /// multi-byte characters (#3226).
    #[test]
    fn a_suffix_match_is_equal_or_ends_after_a_slash(
        word in "[a-z/é世]{0,10}",
        key in "[a-z/é世]{0,6}",
    ) {
        let oracle = word == key || word.ends_with(&format!("/{key}"));
        prop_assert_eq!(suffix_matches(&word, &key), oracle, "{} / {}", word, key);
    }

    /// A word that ends with a slash, or holds `//`, is never a script path, and a word under a
    /// system folder is not one either.
    #[test]
    fn a_double_slash_or_a_trailing_slash_is_not_a_script_path(
        left in "[a-z]{1,5}",
        right in "[a-z]{1,5}",
    ) {
        let double = format!("/{left}//{right}");
        let trailing = format!("/{left}/{right}/");
        prop_assert!(!is_script_path(&double), "{}", double);
        prop_assert!(!is_script_path(&trailing), "{}", trailing);
        let plain = format!("/{left}/{right}");
        prop_assert!(
            is_script_path(&plain) || SYSTEM_DIRS.contains(&left.as_str()),
            "{}",
            plain
        );
        for dir in SYSTEM_DIRS {
            let path = format!("/{dir}/{right}");
            prop_assert!(!is_script_path(&path), "{}", path);
        }
    }

    /// A rooted word inside the cache folder is never an unresolved token, and the same word
    /// outside it is one.
    #[test]
    fn a_word_inside_the_cache_is_never_unresolved(
        dirs in proptest::collection::vec("[a-z]{1,5}", 1..3),
        file in "[a-z]{1,5}\\.sh",
    ) {
        let inside = format!("{CACHE}/{}/{file}", dirs.join("/"));
        let outside = format!("/srv{}/{}/{file}", CACHE, dirs.join("/"));
        prop_assert!(tokens(&format!("bash {inside}")).is_empty(), "{}", inside);
        prop_assert_eq!(tokens(&format!("bash {outside}")), vec![outside.clone()]);
    }

    /// Resolving a command again changes nothing: the first pass writes cache paths, and the
    /// second pass maps each of them to itself.
    #[test]
    fn resolving_twice_equals_resolving_once(
        dirs in proptest::collection::vec("[a-z]{1,5}", 1..3),
        file in "[a-z]{1,5}\\.sh",
        root in "/[a-z]{1,5}",
        short_too in any::<bool>(),
    ) {
        let key = format!("{}/{file}", dirs.join("/"));
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from(&key), PathBuf::from("/src/long.sh"));
        if short_too {
            files.insert(PathBuf::from(&file), PathBuf::from("/src/short.sh"));
        }
        let bundle = Path::new("/bundles/b");
        let mut warned = BTreeSet::new();
        let command = format!("bash {root}/{key} --flag");
        let once = resolve_hook_command(&command, Some(bundle), Path::new(CACHE), &files, &mut warned).unwrap();
        let twice = resolve_hook_command(&once, Some(bundle), Path::new(CACHE), &files, &mut warned).unwrap();
        prop_assert_eq!(&twice, &once, "{}", command);
    }
}
