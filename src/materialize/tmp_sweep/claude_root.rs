//! The roots under `state/tmp` that hold unrelated entries: the per-user TMPDIR root that Claude
//! Code creates (#1379), and the shared `cargo hawk` target folder, with one build per workspace
//! (#3184).
//!
//! A root is never removed, because its mtime does not show use. Its entries are judged one by
//! one, with the same in-use check as the rest of the sweep.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::walk::{Scan, judge_entry, record_failure};

/// The `cargo hawk` target folder. It holds one build folder per workspace.
const HAWK_TARGET_ROOT: &str = "cargo-hawk-target";

/// Whether the entry `name` is a root whose entries are judged one by one.
pub(super) fn is_split_root(name: &std::ffi::OsStr) -> bool {
    name == HAWK_TARGET_ROOT || is_claude_root(name)
}

/// Judge the entries one level down inside a split root. The root itself is never a candidate.
pub(super) fn scan_split_root(
    split_root: &Path,
    older_than: Duration,
    in_use: &[PathBuf],
    now: SystemTime,
    scan: &mut Scan,
) {
    let entries = match crate::paths::read_dir_optional(split_root) {
        Ok(Some(entries)) => entries,
        Ok(None) => return,
        Err(e) => {
            record_failure(split_root, &e, scan);
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                record_failure(split_root, &e, scan);
                continue;
            }
        };
        let path = split_root.join(entry.file_name());
        if in_use.iter().any(|used| used.starts_with(&path)) {
            scan.kept += 1;
            continue;
        }
        match entry.file_type() {
            Ok(file_type) => judge_entry(&path, file_type.is_dir(), older_than, now, scan),
            Err(e) => record_failure(&path, &e, scan),
        }
    }
}

/// Whether `name` is `claude-<uid>`, the per-user TMPDIR root that Claude Code creates.
fn is_claude_root(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("claude-"))
        .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs::{self, File};
    use std::time::SystemTime;

    use crate::materialize::tmp_sweep::sweep;
    use crate::materialize::tmp_sweep::tests::{DAY, age_tree, old};

    #[test]
    fn a_claude_code_root_is_kept_even_when_old() {
        let root = tempfile::tempdir().unwrap();
        let claude = root.path().join("claude-501");
        fs::create_dir(&claude).unwrap();
        age_tree(&claude, old());

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert!(report.removed.is_empty());
        assert!(claude.exists(), "the per-user TMPDIR root was removed");
    }

    #[test]
    fn an_old_entry_inside_a_claude_code_root_is_removed_and_a_fresh_one_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let claude = root.path().join("claude-501");
        fs::create_dir(&claude).unwrap();
        let old_child = claude.join("shell-old");
        fs::create_dir(&old_child).unwrap();
        fs::write(old_child.join("snapshot"), b"s").unwrap();
        age_tree(&old_child, old());
        let fresh_child = claude.join("shell-fresh");
        fs::create_dir(&fresh_child).unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert_eq!(report.removed.len(), 1, "removed: {:?}", report.removed);
        assert!(
            !old_child.exists(),
            "the old entry inside the root survived"
        );
        assert!(
            fresh_child.exists(),
            "a fresh entry inside the root was removed"
        );
        assert!(claude.exists(), "the per-user TMPDIR root was removed");
    }

    #[test]
    fn an_old_entry_inside_a_claude_code_root_with_a_path_in_use_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let claude = root.path().join("claude-501");
        fs::create_dir(&claude).unwrap();
        let busy = claude.join("shell-busy");
        fs::create_dir(&busy).unwrap();
        let cwd_file = busy.join("cwd.txt");
        fs::write(&cwd_file, b"c").unwrap();
        age_tree(&busy, old());

        let report = sweep(root.path(), DAY * 7, std::slice::from_ref(&cwd_file), false).unwrap();

        assert!(
            report.removed.is_empty(),
            "removed a busy entry: {:?}",
            report.removed
        );
        assert!(cwd_file.exists());
    }

    #[test]
    fn an_entry_with_a_future_mtime_inside_a_claude_code_root_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let claude = root.path().join("claude-501");
        fs::create_dir(&claude).unwrap();
        let future = claude.join("future.txt");
        fs::write(&future, b"f").unwrap();
        File::open(&future)
            .unwrap()
            .set_modified(SystemTime::now() + DAY)
            .unwrap();

        let report = sweep(root.path(), DAY * 7, &[], false).unwrap();

        assert!(report.removed.is_empty());
        assert!(future.exists());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod name_tests {
    use proptest::prelude::*;

    use super::{is_claude_root, is_split_root};

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_never_a_split_root() {
        use std::os::unix::ffi::OsStrExt;
        assert!(!is_split_root(std::ffi::OsStr::from_bytes(b"claude-5\xff")));
        assert!(!is_split_root(std::ffi::OsStr::from_bytes(
            b"cargo-hawk-target\xff"
        )));
    }

    #[test]
    fn only_a_claude_prefix_with_digits_is_a_root() {
        for yes in ["claude-501", "claude-0"] {
            assert!(is_claude_root(std::ffi::OsStr::new(yes)), "{yes}");
        }
        for no in [
            "claude-",
            "claude-x",
            "claude-5a",
            "claude501",
            "Claude-501",
            "shell-501",
        ] {
            assert!(!is_claude_root(std::ffi::OsStr::new(no)), "{no}");
        }
    }

    proptest! {
        /// A name without the `claude-` prefix is never a root, so the sweep judges it normally.
        #[test]
        fn a_name_without_the_prefix_is_never_a_root(name in "[a-z0-9-]{0,12}") {
            prop_assume!(!name.starts_with("claude-"));

            prop_assert!(!is_claude_root(std::ffi::OsStr::new(&name)));
        }

        /// A split root is the hawk target folder, or `claude-` and ASCII digits, and nothing else.
        #[test]
        fn a_split_root_is_the_hawk_folder_or_a_claude_uid(
            name in prop_oneof![
                Just("cargo-hawk-target".to_owned()),
                "claude-[0-9]{0,6}",
                "[a-z0-9-]{0,20}",
                "claude-[0-9]{1,3}[a-z\u{0660}-\u{0669}]{1,2}",
            ],
        ) {
            let digits = name.strip_prefix("claude-").is_some_and(|uid| {
                !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit())
            });
            let expected = name == "cargo-hawk-target" || digits;
            prop_assert_eq!(is_split_root(std::ffi::OsStr::new(&name)), expected);
        }
    }
}
