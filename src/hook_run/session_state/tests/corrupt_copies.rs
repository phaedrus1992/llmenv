//! Corrupt move-aside copies are pruned with the stale state files (#3372).

use std::path::Path;
use std::time::{Duration, SystemTime};

use proptest::prelude::*;
use tempfile::TempDir;

use super::super::{AgeSource, is_prunable_state_file, prune_stale_json_files_with};

proptest! {
    /// Only a `.json` file, or a corrupt copy with a stem and an optional decimal index, is
    /// prunable. The oracle is the spec, not the implementation.
    #[test]
    fn only_json_files_and_corrupt_copies_with_a_stem_are_prunable(
        stem in "[a-z]{0,4}",
        suffix in r"(\.json|\.json\.corrupt|\.json\.corrupt\.[0-9]{0,2}|\.json\.corrupt[a-z.]{0,2}|\.txt)",
    ) {
        let name = format!("{stem}{suffix}");
        let numbered = suffix.strip_prefix(".json.corrupt.").is_some_and(|d| {
            !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
        });
        let expected =
            !stem.is_empty() && (suffix == ".json" || suffix == ".json.corrupt" || numbered);
        prop_assert_eq!(is_prunable_state_file(Path::new(&name)), expected, "{}", name);
    }
}

#[test]
fn prunes_stale_corrupt_copies_and_keeps_fresh_ones() {
    let dir = TempDir::new().expect("test");
    let old_ft =
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(10 * 86_400));
    let stale = ["s1.json.corrupt", "s1.json.corrupt.3"].map(|n| dir.path().join(n));
    let fresh = dir.path().join("s2.json.corrupt");
    for path in &stale {
        std::fs::write(path, "bad").expect("test");
        filetime::set_file_mtime(path, old_ft).expect("test");
    }
    std::fs::write(&fresh, "bad").expect("test");

    prune_stale_json_files_with(dir.path(), 7, AgeSource::MtimeOnly);

    for path in &stale {
        assert!(
            !path.exists(),
            "stale copy must be pruned: {}",
            path.display()
        );
    }
    assert!(fresh.exists(), "fresh copy must survive pruning");
}

#[test]
fn only_json_files_and_numbered_corrupt_copies_are_prunable() {
    for name in ["a.json", "a.json.corrupt", "a.json.corrupt.12"] {
        assert!(is_prunable_state_file(Path::new(name)), "{name}");
    }
    for name in [
        "a.txt",
        "a.json.corrupt.bak",
        "a.json.corrupt.",
        ".json.corrupt",
        "a.json.corruptx",
    ] {
        assert!(!is_prunable_state_file(Path::new(name)), "{name}");
    }
}
