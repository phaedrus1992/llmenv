//! XDG paths and path helpers.

use std::env::VarError;
use std::path::{Path, PathBuf};

mod atomic_write;
mod bounded_read;
mod names;
mod owner_only;
mod tilde;
mod unsynced_write;
mod write_stats;
pub use tilde::expand_tilde;
use tilde::expand_tilde_with_env;
pub use write_stats::{WriteStats, snapshot as write_stats_snapshot};
pub mod program;
pub use atomic_write::{resolve_user_link, write_owner_only_atomic, write_user_file_atomic};
pub use bounded_read::{check_regular_file, read_file_bounded, read_open_file, read_text_bounded};
pub use names::{
    cwd_under_prefix, has_parent_component, is_unsafe_join_target, is_valid_short_name,
};
pub use owner_only::{
    create_dir_owner_only, is_executable, open_append_owner_only, write_owner_only,
    write_owner_only_executable, write_owner_only_like,
};
pub use unsynced_write::write_owner_only_atomic_unsynced;

/// `read_dir` that treats a missing directory as "nothing to iterate":
/// returns `Ok(None)` on `NotFound` but propagates every other I/O error (e.g.
/// a permission denial), with `reading <dir>` context. Use instead of an
/// `exists()`-then-`read_dir` guard, which collapses *all* stat failures —
/// including `EACCES` — to "absent" and so silently skips a directory the
/// caller can't read (#918).
///
/// # Errors
/// Returns any `read_dir` error other than `NotFound` (e.g. permission denied,
/// or the path is not a directory).
pub fn read_dir_optional(dir: &Path) -> anyhow::Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", dir.display()))),
    }
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    ensure_utf8_override("LLMENV_CONFIG_DIR")?;
    config_dir_with_env(&|name| std::env::var(name).ok())
}

/// Fails when `name` is set to a value that is not valid UTF-8. The env closures read such a value
/// as unset, so the default path would be used with no message (#2607).
fn ensure_utf8(name: &str, value: Option<&std::ffi::OsStr>) -> anyhow::Result<()> {
    match value.map(std::ffi::OsStr::to_str) {
        Some(None) => {
            anyhow::bail!("{name} is not valid UTF-8; set {name} to a UTF-8 value and retry")
        }
        _ => Ok(()),
    }
}

/// Checks the override `name`, and `HOME` when the resolved path reads it (#2607).
fn ensure_utf8_override(name: &str) -> anyhow::Result<()> {
    let value = std::env::var_os(name);
    ensure_utf8(name, value.as_deref())?;
    if home_is_read(value.as_deref()) {
        ensure_utf8("HOME", std::env::var_os("HOME").as_deref())?;
    }
    Ok(())
}

/// Whether the resolved path reads `HOME`: when the override is unset or empty, or when it is a
/// `~` path that is expanded. An absolute override never reads it.
fn home_is_read(override_value: Option<&std::ffi::OsStr>) -> bool {
    match override_value.filter(|v| !v.is_empty()) {
        None => true,
        Some(value) => value.to_str().is_some_and(|s| s.starts_with('~')),
    }
}

/// Adapts an Option-based env provider to the Result-based one that
/// [`expand_tilde_with_env`] takes. A non-UTF-8 `HOME` never reaches this adapter in
/// production, because [`ensure_utf8_override`] rejects it first.
fn present_only(
    get_env: &impl Fn(&str) -> Option<String>,
) -> impl Fn(&str) -> Result<String, VarError> + '_ {
    move |name| get_env(name).ok_or(VarError::NotPresent)
}

/// [`config_dir`] with an injectable env-var provider so tests can exercise
/// the set-but-empty override case without mutating real process env vars.
fn config_dir_with_env(get_env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    // A set-but-empty override (`LLMENV_CONFIG_DIR=`) must fall through to the
    // `$HOME` default rather than resolving to a relative `PathBuf::from("")` (#1111).
    if let Some(dir) = get_env("LLMENV_CONFIG_DIR").filter(|d| !d.is_empty()) {
        // Same bug class as #1910: a tilde-prefixed override is a directory
        // path, not a literal string, and must be expanded like any other.
        Ok(PathBuf::from(expand_tilde_with_env(
            &dir,
            &present_only(get_env),
        )?))
    } else {
        // A set-but-empty `HOME` has the identical failure mode as the override
        // above (a relative `PathBuf::from("")`-derived path) — treat it as
        // unset too rather than fixing only the override branch.
        let home = get_env("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("environment variable not found: HOME"))?;
        Ok(PathBuf::from(home).join(".config/llmenv"))
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

pub fn state_dir() -> anyhow::Result<PathBuf> {
    ensure_utf8_override("LLMENV_STATE_DIR")?;
    state_dir_with_env(&|name| std::env::var(name).ok())
}

/// [`state_dir`] with an injectable env-var provider so tests can exercise
/// the set-but-empty override case without mutating real process env vars.
fn state_dir_with_env(get_env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    // A set-but-empty override (`LLMENV_STATE_DIR=`) must fall through to the
    // `$HOME` default rather than resolving to a relative `PathBuf::from("")` (#1111).
    if let Some(dir) = get_env("LLMENV_STATE_DIR").filter(|d| !d.is_empty()) {
        // Same bug class as #1910: a tilde-prefixed override is a directory
        // path, not a literal string, and must be expanded like any other.
        Ok(PathBuf::from(expand_tilde_with_env(
            &dir,
            &present_only(get_env),
        )?))
    } else {
        // Same reasoning as `config_dir_with_env`: a set-but-empty `HOME` must
        // also be treated as unset, not just the override.
        let home = get_env("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("environment variable not found: HOME"))?;
        Ok(PathBuf::from(home).join(".local/state/llmenv"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ensure_utf8_rejects_a_value_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(&[0xff, 0xfe]);
        let err = ensure_utf8("LLMENV_STATE_DIR", Some(raw)).unwrap_err();
        assert!(
            err.to_string()
                .contains("LLMENV_STATE_DIR is not valid UTF-8"),
            "{err}"
        );
    }

    #[test]
    fn home_is_read_only_when_the_override_does_not_resolve_the_path() {
        use std::ffi::OsStr;
        assert!(home_is_read(None));
        assert!(home_is_read(Some(OsStr::new(""))));
        assert!(home_is_read(Some(OsStr::new("~/llmenv"))));
        assert!(!home_is_read(Some(OsStr::new("/srv/llmenv"))));
    }

    #[test]
    fn ensure_utf8_accepts_unset_and_utf8_values() {
        assert!(ensure_utf8("HOME", None).is_ok());
        assert!(ensure_utf8("HOME", Some(std::ffi::OsStr::new("/home/u"))).is_ok());
    }

    #[test]
    fn state_dir_with_env_empty_override_falls_through_to_home() {
        let get_env = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let result = state_dir_with_env(&get_env).unwrap();
        assert_eq!(result, PathBuf::from("/home/testuser/.local/state/llmenv"));
    }

    #[test]
    fn state_dir_with_env_empty_equals_unset() {
        let unset = |name: &str| match name {
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let empty = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir_with_env(&unset).unwrap(),
            state_dir_with_env(&empty).unwrap()
        );
    }

    #[test]
    fn state_dir_with_env_non_empty_override_used_verbatim() {
        let get_env = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some("/custom/state".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/custom/state")
        );
    }

    #[test]
    fn state_dir_with_env_tilde_override_is_expanded() {
        // Same bug class as #1910's Config::load fix: LLMENV_STATE_DIR=~/foo
        // must not be treated as a literal directory named "~".
        let get_env = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some("~/custom-state".to_string()),
            "HOME" => Some("/home/user".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/home/user/custom-state")
        );
    }

    #[test]
    fn state_dir_with_env_empty_home_errors_instead_of_relative_path() {
        let get_env = |name: &str| match name {
            "HOME" => Some(String::new()),
            _ => None,
        };
        let result = state_dir_with_env(&get_env);
        assert!(
            result.is_err(),
            "a set-but-empty HOME must error, not resolve to a relative path: {result:?}"
        );
    }

    #[test]
    fn config_dir_with_env_empty_override_falls_through_to_home() {
        let get_env = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let result = config_dir_with_env(&get_env).unwrap();
        assert_eq!(result, PathBuf::from("/home/testuser/.config/llmenv"));
    }

    #[test]
    fn config_dir_with_env_empty_equals_unset() {
        let unset = |name: &str| match name {
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let empty = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        assert_eq!(
            config_dir_with_env(&unset).unwrap(),
            config_dir_with_env(&empty).unwrap()
        );
    }

    #[test]
    fn config_dir_with_env_non_empty_override_used_verbatim() {
        let get_env = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some("/custom/config".to_string()),
            _ => None,
        };
        assert_eq!(
            config_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/custom/config")
        );
    }

    #[test]
    fn config_dir_with_env_tilde_override_is_expanded() {
        // Same bug class as #1910's Config::load fix: LLMENV_CONFIG_DIR=~/foo
        // must not be treated as a literal directory named "~".
        let get_env = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some("~/custom-config".to_string()),
            "HOME" => Some("/home/user".to_string()),
            _ => None,
        };
        assert_eq!(
            config_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/home/user/custom-config")
        );
    }

    #[test]
    fn config_dir_with_env_empty_home_errors_instead_of_relative_path() {
        let get_env = |name: &str| match name {
            "HOME" => Some(String::new()),
            _ => None,
        };
        let result = config_dir_with_env(&get_env);
        assert!(
            result.is_err(),
            "a set-but-empty HOME must error, not resolve to a relative path: {result:?}"
        );
    }

    #[test]
    fn read_dir_optional_returns_none_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            read_dir_optional(&tmp.path().join("nope"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn read_dir_optional_returns_some_for_present_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_dir_optional(tmp.path()).unwrap().is_some());
    }

    // #918: a non-NotFound I/O error (EACCES) propagates rather than being
    // masked as an absent directory the way an exists() stat would.
    #[cfg(unix)]
    #[test]
    fn read_dir_optional_propagates_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_dir_optional(&child);
        let readable_anyway = std::fs::read_dir(&child).is_ok();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "permission error must propagate, got {result:?}"
        );
    }

    #[test]
    fn is_valid_short_name_accepts_alphanumeric_dot_underscore_dash() {
        for name in ["superpowers", "context-mode", "v1.2.3", "foo_bar", "a"] {
            assert!(is_valid_short_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn is_valid_short_name_rejects_empty_dot_dotdot_and_leading_dash() {
        for name in ["", ".", "..", "-evil"] {
            assert!(!is_valid_short_name(name), "{name} should be rejected");
        }
    }

    #[test]
    fn is_valid_short_name_rejects_path_separator() {
        for name in ["foo/bar", "foo\\bar"] {
            assert!(!is_valid_short_name(name), "{name} should be rejected");
        }
    }

    #[test]
    fn is_valid_short_name_rejects_control_and_non_ascii_characters() {
        // #534: a blocklist-style check misses Unicode formatting characters
        // (zero-width space, RTL override) that an allowlist closes by construction.
        for name in ["foo\0bar", "foo\u{200B}bar", "foo\u{202E}bar", "café"] {
            assert!(!is_valid_short_name(name), "{name} should be rejected");
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_is_valid_short_name_no_panic(s in ".*") {
            let _ = is_valid_short_name(&s);
        }

        #[test]
        fn prop_valid_names_are_ascii_alphanumeric_subset(
            name in "[a-zA-Z][a-zA-Z0-9._-]{0,30}",
        ) {
            if name != "." && name != ".." && !name.starts_with('-') {
                proptest::prop_assert!(is_valid_short_name(&name));
            }
        }

        #[test]
        fn prop_non_ascii_always_rejected(s in "[^\x00-\x7F]+") {
            proptest::prop_assert!(!is_valid_short_name(&s));
        }

        #[test]
        fn prop_valid_short_name_is_never_an_unsafe_join_target(
            name in "[a-zA-Z][a-zA-Z0-9._-]{0,30}",
        ) {
            if is_valid_short_name(&name) {
                proptest::prop_assert!(!is_unsafe_join_target(&name));
            }
        }
    }

    #[test]
    fn cwd_under_prefix_respects_component_boundary() {
        assert!(cwd_under_prefix("/home/alice/git/x", "/home/alice/git/x"));
        assert!(cwd_under_prefix(
            "/home/alice/git/x/sub",
            "/home/alice/git/x"
        ));
        assert!(!cwd_under_prefix(
            "/home/alice/git/xyz",
            "/home/alice/git/x"
        ));
        assert!(!cwd_under_prefix("/home/alice", "/home/alice/git"));
    }

    #[test]
    fn has_parent_component_detects_traversal_substring_misses() {
        // Trailing `..` with no slash — substring check for "../" misses this.
        assert!(has_parent_component("foo/.."));
        assert!(has_parent_component(".."));
        assert!(has_parent_component("/foo/../bar"));
        assert!(has_parent_component("a/b/../c"));
    }

    #[test]
    fn has_parent_component_allows_safe_paths() {
        assert!(!has_parent_component("/home/alice/.cache/llmenv"));
        assert!(!has_parent_component("relative/path"));
        assert!(!has_parent_component("~/.cache/llmenv"));
        // A `..` embedded in a name is not a parent component.
        assert!(!has_parent_component("/foo/..bar/baz"));
        assert!(!has_parent_component("file..txt"));
        assert!(!has_parent_component(""));
    }

    #[test]
    fn has_parent_component_does_not_check_absolute_paths() {
        // Documents that has_parent_component alone is INSUFFICIENT for
        // safe-join validation. Callers must use is_unsafe_join_target.
        assert!(!has_parent_component("/etc/passwd"));
        assert!(!has_parent_component("/abs/secret"));
    }

    #[test]
    fn is_unsafe_join_target_rejects_traversal_and_absolute() {
        // Parent components — same as has_parent_component.
        assert!(is_unsafe_join_target(".."));
        assert!(is_unsafe_join_target("foo/.."));
        assert!(is_unsafe_join_target("a/b/../c"));
        // Absolute paths — would escape via Path::join semantics.
        assert!(is_unsafe_join_target("/etc/passwd"));
        assert!(is_unsafe_join_target("/abs"));
        // Safe: plain relative paths.
        assert!(!is_unsafe_join_target("rel/path"));
        assert!(!is_unsafe_join_target("file.txt"));
        assert!(!is_unsafe_join_target("a/b/c"));
        // Embedded `..` in a name is not a parent component.
        assert!(!is_unsafe_join_target("file..txt"));
    }

    #[test]
    fn tilde_passthrough_for_absolute_and_relative() {
        // Tests the non-HOME-dependent branches.
        assert_eq!(
            expand_tilde("/abs/path").expect("no HOME read"),
            "/abs/path"
        );
        assert_eq!(expand_tilde("rel/path").expect("no HOME read"), "rel/path");
        assert_eq!(expand_tilde("").expect("no HOME read"), "");
    }

    #[test]
    fn expand_tilde_with_env_unset_or_empty_home_is_an_error_naming_home() {
        let unset = |_: &str| Err(VarError::NotPresent);
        let empty = |name: &str| match name {
            "HOME" => Ok(String::new()),
            _ => Err(VarError::NotPresent),
        };
        for get_env in [&unset as &dyn Fn(&str) -> Result<String, VarError>, &empty] {
            for input in ["~", "~/foo"] {
                let err = expand_tilde_with_env(input, &get_env)
                    .expect_err("a ~ path with no HOME must fail");
                assert!(
                    err.to_string().contains("HOME is not set"),
                    "{input}: {err}"
                );
            }
        }
    }

    #[test]
    fn config_and_state_dir_tilde_overrides_fail_without_home() {
        let override_only = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some("~/cfg".to_string()),
            "LLMENV_STATE_DIR" => Some("~/state".to_string()),
            _ => None,
        };
        let config_err = config_dir_with_env(&override_only).expect_err("no HOME for ~/cfg");
        assert!(
            config_err.to_string().contains("HOME is not set"),
            "{config_err}"
        );
        let state_err = state_dir_with_env(&override_only).expect_err("no HOME for ~/state");
        assert!(
            state_err.to_string().contains("HOME is not set"),
            "{state_err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn expand_tilde_non_utf8_home_is_an_error_that_names_home() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = std::ffi::OsString::from_vec(vec![0x2f, 0x80]);
        let bad_home = |_: &str| Err(VarError::NotUnicode(raw.clone()));

        for input in ["~", "~/cache"] {
            let err = expand_tilde_with_env(input, &bad_home)
                .expect_err("a non-UTF-8 HOME must not expand silently");
            let msg = format!("{err:#}");
            assert!(msg.contains("HOME"), "{msg}");
            assert!(msg.contains(input), "error must name the input: {msg}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn expand_tilde_non_utf8_home_leaves_other_paths_alone() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = std::ffi::OsString::from_vec(vec![0x80]);
        let bad_home = |_: &str| Err(VarError::NotUnicode(raw.clone()));

        assert_eq!(
            expand_tilde_with_env("/abs/path", &bad_home).expect("absolute paths skip HOME"),
            "/abs/path"
        );
        assert_eq!(
            expand_tilde_with_env("rel/path", &bad_home).expect("relative paths skip HOME"),
            "rel/path"
        );
    }

    #[test]
    fn expand_tilde_bare_tilde_equals_home() {
        let home = std::env::var("HOME").expect("HOME must be set; expand_tilde relies on it");
        let result = expand_tilde("~").expect("HOME is UTF-8 in the test environment");
        assert_eq!(result, home);
        assert!(!result.ends_with('/'));
    }

    // ===== Property tests for atomic-write byte roundtrip (#156 / #157) =====

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn has_parent_component_no_panic(s in ".*") {
            let _ = has_parent_component(&s);
        }

        #[test]
        fn is_unsafe_join_target_no_panic(s in ".*") {
            let _ = is_unsafe_join_target(&s);
        }

        #[test]
        fn has_parent_implies_unsafe_join(s in ".*") {
            // is_unsafe_join_target is a strict superset of has_parent_component
            if has_parent_component(&s) {
                prop_assert!(is_unsafe_join_target(&s),
                    "has_parent_component=true but is_unsafe_join_target=false for: {s:?}");
            }
        }

        #[test]
        fn absolute_path_always_unsafe_join(s in "/.*") {
            prop_assert!(is_unsafe_join_target(&s),
                "absolute path not flagged: {s:?}");
        }

        #[test]
        fn expand_tilde_passthrough_non_tilde(s in "[^~].*") {
            prop_assert_eq!(expand_tilde(&s).expect("non-tilde input never reads HOME"), s);
        }

        #[test]
        fn expand_tilde_never_panics(s in ".*") {
            let _ = expand_tilde(&s);
        }

        #[test]
        fn expand_tilde_with_env_follows_home(
            rest in "[a-z0-9/_.-]{0,20}",
            // A non-empty HOME: an empty one is an error, covered by the unit test below.
            home in "[a-z0-9/_.-]{1,20}",
        ) {
            let input = format!("~/{rest}");
            let read = |_: &str| Ok(home.clone());
            let got = expand_tilde_with_env(&input, &read).expect("a UTF-8 HOME never errors");
            if home.is_empty() {
                prop_assert_eq!(got, input);
            } else {
                prop_assert_eq!(got, format!("{home}/{rest}"));
            }
        }

        #[test]
        fn expand_tilde_with_env_unset_home_errors_only_for_a_tilde_input(s in ".*") {
            let unset = |_: &str| Err(VarError::NotPresent);
            let reads_home = s == "~" || s.starts_with("~/");
            prop_assert_eq!(expand_tilde_with_env(&s, &unset).is_err(), reads_home);
        }

        #[test]
        fn expand_tilde_with_env_errors_only_for_a_tilde_input(s in ".*") {
            let bad = |_: &str| Err(VarError::NotUnicode(std::ffi::OsString::from("x")));
            let reads_home = s == "~" || s.starts_with("~/");
            prop_assert_eq!(expand_tilde_with_env(&s, &bad).is_err(), reads_home);
        }

        #[test]
        fn expand_tilde_slash_contains_home_and_rest(rest in "[a-z0-9/_.-]{0,20}") {
            let home_result = std::env::var("HOME");
            prop_assume!(home_result.is_ok());
            let home = home_result.unwrap();
            let input = format!("~/{rest}");
            let result = expand_tilde(&input).expect("HOME is UTF-8 in the test environment");
            prop_assert!(result.starts_with(&home),
                "expected {result} to start with home={home}");
            prop_assert!(result.ends_with(&rest) || rest.is_empty(),
                "expected {result} to end with rest={rest}");
        }

        #[test]
        fn cwd_under_prefix_reflexive(p in "/[a-z/]{1,20}") {
            prop_assert!(cwd_under_prefix(&p, &p));
        }

        #[test]
        fn cwd_under_prefix_child_under_parent(
            parent in "/[a-z]{1,10}",
            child in "[a-z]{1,10}",
        ) {
            let full = format!("{parent}/{child}");
            prop_assert!(cwd_under_prefix(&full, &parent));
        }

        #[test]
        fn cwd_under_prefix_no_string_prefix_false_positive(
            base in "[a-z]{2,8}",
            extra in "[a-z]{1,4}",
        ) {
            let cwd = format!("/{base}{extra}");
            let prefix = format!("/{base}");
            prop_assert!(!cwd_under_prefix(&cwd, &prefix));
        }

        #[test]
        fn cwd_under_prefix_never_panics(cwd in ".*", prefix in ".*") {
            let _ = cwd_under_prefix(&cwd, &prefix);
        }

        #[test]
        fn cwd_under_prefix_transitive(
            root in "/[a-z]{1,6}",
            mid in "[a-z]{1,6}",
            leaf in "[a-z]{1,6}",
        ) {
            let b = format!("{root}/{mid}");
            let a = format!("{b}/{leaf}");
            prop_assert!(cwd_under_prefix(&b, &root));
            prop_assert!(cwd_under_prefix(&a, &b));
            prop_assert!(cwd_under_prefix(&a, &root));
        }

        #[test]
        fn cwd_under_prefix_not_symmetric(
            parent in "/[a-z]{1,10}",
            child in "[a-z]{1,10}",
        ) {
            let child_path = format!("{parent}/{child}");
            prop_assert!(!cwd_under_prefix(&parent, &child_path));
        }

        #[test]
        fn has_parent_component_safe_components(
            a in "[a-z]{1,8}",
            b in "[a-z]{1,8}",
        ) {
            let path = format!("{a}/{b}");
            prop_assert!(!has_parent_component(&path));
        }

        #[test]
        fn is_unsafe_join_target_join_safety(p in "[a-z/]{1,20}") {
            prop_assume!(!is_unsafe_join_target(&p));
            let joined = std::path::PathBuf::from("/base").join(&p);
            prop_assert!(joined.starts_with("/base"), "join escaped base: {:?}", joined);
        }

        // Arbitrary byte payloads written through write_owner_only_atomic must
        // round-trip exactly via fs::read. Catches truncation, encoding, or
        // mid-write corruption regressions across the full u8 range including
        // NUL bytes and high-bit values.
        #[test]
        fn atomic_write_byte_roundtrip(payload in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let path = dir.path().join("payload.bin");
            write_owner_only_atomic(&path, &payload).expect("atomic write");
            let read = std::fs::read(&path).expect("read");
            prop_assert_eq!(payload, read);
        }

        // Repeated overwrites must end with the final payload exactly — no
        // residual bytes from prior writes, no torn state, no permission
        // escalation.
        #[test]
        fn atomic_write_overwrite_idempotent(
            first in proptest::collection::vec(any::<u8>(), 0..4096),
            second in proptest::collection::vec(any::<u8>(), 0..4096),
        ) {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let path = dir.path().join("payload.bin");
            write_owner_only_atomic(&path, &first).expect("write 1");
            write_owner_only_atomic(&path, &second).expect("write 2");
            let read = std::fs::read(&path).expect("read");
            prop_assert_eq!(second, read);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
                prop_assert_eq!(mode & 0o077, 0, "group/other bits set after overwrite: {:o}", mode);
            }
        }
    }
}
