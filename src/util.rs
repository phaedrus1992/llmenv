pub(crate) use llmenv_util::normalize_yaml;
pub(crate) mod semver;
pub use llmenv_util::{
    dedup, escape_control, expand_env_refs, merge_json, merge_yaml, strip_unsafe_chars,
};

/// A control character, or an invisible format character that can reorder or hide text.
fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

/// `s` in single quotes, with each embedded single quote closed, escaped, and reopened. The
/// shell expands nothing inside, so the text reaches the command as it is.
pub(crate) fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `path` as one shell word, so a command in a message can be pasted as it is.
pub(crate) fn shell_word(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c));
    if plain {
        text
    } else if text.is_empty() {
        // An empty word would vanish from the pasted command, so it is written as a quoted empty.
        "''".to_string()
    } else {
        single_quote(&text)
    }
}

/// Escape every C0/C1 control character in `s` (`char::is_control`) so it's
/// safe to print verbatim to a terminal (#1076). Config-derived strings
/// (`native_permissions.*` keys, permission tool names and patterns,
/// bundle/marketplace names) can originate from a shared or marketplace
/// `bundle.yaml` since #1072 widened validation to the merged manifest — a
/// key or pattern containing an ANSI escape or a carriage return could
/// otherwise rewrite or hide surrounding `doctor`/`export` output on
/// someone else's terminal. No exceptions for `\n`/`\t`: a legitimate
/// config key, tool name, or pattern has no reason to contain either.
///
/// Invisible Unicode format characters are escaped too (#2669 review). They are not control
/// characters, but a bidi override (U+202E) can reorder the text around a key, and a
/// zero-width character can hide part of it.
///
/// Returns `s` unchanged (no allocation) when nothing needs escaping.
pub(crate) fn display_safe(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(needs_escape) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if needs_escape(c) {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Lexically normalize a path: resolve `..` and `.` components without touching
/// the filesystem. This prevents `..`-based traversal bypasses when checking
/// path prefixes on paths that may not exist yet.
pub(crate) fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Truncate `s` to at most `max_len` **characters** (not bytes), appending
/// `…` (U+2026, itself counted within `max_len`) when truncation occurs.
/// UTF-8-boundary-safe: always truncates on a `char` boundary since it
/// iterates `chars()` rather than slicing bytes.
#[must_use]
pub(crate) fn truncate_ellipsis(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    if max_len == 0 {
        return String::new();
    }
    let keep = max_len.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Whether `expanded_path` lies within `cache_root`. Components are compared, not text, so
/// `/cache2` is not within `/cache`. `..` components are normalized lexically, so
/// `/cache/llmenv/../../../etc/passwd` cannot pass. Links are resolved in the part of each path
/// that exists, so a link outside the root that points into it counts as within, and a new file
/// under a link that leaves the root does not.
#[must_use]
pub(crate) fn is_within(cache_root: &std::path::Path, expanded_path: &str) -> bool {
    let path = std::path::Path::new(expanded_path);
    resolve_existing(path).starts_with(resolve_existing(cache_root))
}

/// `path` normalized, with links resolved in its longest prefix that exists. The rest is joined
/// on as it is, because it does not exist yet.
fn resolve_existing(path: &std::path::Path) -> std::path::PathBuf {
    let normalized = normalize_path(path);
    let mut prefix = normalized.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(prefix) {
            resolved.extend(rest.iter().rev());
            return resolved;
        }
        match (prefix.file_name(), prefix.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                prefix = parent;
            }
            _ => return normalized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_safe_returns_borrowed_for_plain_text() {
        assert!(matches!(
            display_safe("plain text"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn display_safe_escapes_ansi_escape() {
        let out = display_safe("Bash(\x1b[2Kevil*)");
        assert!(!out.contains('\x1b'));
        assert!(out.contains("\\u{001b}"));
    }

    #[test]
    fn display_safe_escapes_bidi_override_and_zero_width() {
        let out = display_safe("key\u{202E}evil\u{200B}");
        assert!(!out.contains('\u{202E}'));
        assert!(!out.contains('\u{200B}'));
        assert!(out.contains("\\u{202e}"));
        assert!(out.contains("\\u{200b}"));
    }

    #[test]
    fn display_safe_keeps_ordinary_non_ascii_text() {
        assert!(matches!(
            display_safe("naïve 日本語"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn display_safe_escapes_carriage_return() {
        let out = display_safe("native_permissions\rhidden");
        assert!(!out.contains('\r'));
        assert!(out.contains("\\u{000d}"));
    }

    #[test]
    fn display_safe_escapes_newline_and_tab_too() {
        // No exception for \n/\t: a config key/tool name/pattern has no
        // legitimate reason to contain either.
        let out = display_safe("a\nb\tc");
        assert!(!out.contains('\n') && !out.contains('\t'));
    }

    proptest::proptest! {
        #[test]
        fn display_safe_never_panics(s in ".{0,100}") {
            let _ = display_safe(&s);
        }

        #[test]
        fn display_safe_output_never_contains_a_raw_control_char(s in ".{0,100}") {
            let out = display_safe(&s);
            proptest::prop_assert!(!out.chars().any(char::is_control));
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
#[test]
#[expect(clippy::unwrap_used, reason = "test code")]
fn a_link_into_the_root_counts_as_within() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cache");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("settings.json"), "{}").unwrap();
    let link = dir.path().join("outside-link.json");
    std::os::unix::fs::symlink(root.join("settings.json"), &link).unwrap();
    assert!(is_within(&root, &link.display().to_string()));
    assert!(!is_within(
        &root,
        &dir.path().join("other").display().to_string()
    ));
}

#[cfg(all(test, unix))]
#[test]
#[expect(clippy::unwrap_used, reason = "test code")]
fn a_new_file_under_a_link_out_of_the_root_is_not_within() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cache");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    let new_file = root.join("escape").join("new.json");
    assert!(!is_within(&root, &new_file.display().to_string()));
    let new_inside = root.join("sub").join("new.json");
    assert!(is_within(&root, &new_inside.display().to_string()));
    let missing_root = dir.path().join("not-yet");
    assert!(is_within(
        &missing_root,
        &missing_root.join("a").display().to_string()
    ));
}

#[cfg(test)]
mod normalize_props {
    use proptest::prelude::*;
    use std::path::{Component, PathBuf};

    use super::normalize_path;

    proptest! {
        #[test]
        fn a_normalized_path_has_no_dot_parts_and_is_stable(
            parts in proptest::collection::vec("(\\.\\.|\\.|[a-z]{1,4})", 0..8),
        ) {
            let path: PathBuf = std::iter::once("/".to_string()).chain(parts).collect();
            let once = normalize_path(&path);
            prop_assert!(
                once.components()
                    .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)),
                "{}",
                once.display()
            );
            prop_assert_eq!(normalize_path(&once), once);
        }
    }
}
