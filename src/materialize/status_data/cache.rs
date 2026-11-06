//! Prunable-cache size for the status line. The size comes from a dry-run prune scan, sized with
//! `dir_size`, so reading the stat never deletes anything.

use std::path::Path;

use super::CacheData;
use crate::config::HashingMode;
use crate::materialize::dir_size::{DirSize, dir_size};

/// Best-effort prunable-bytes estimate: a dry-run `StaleOnly` prune scan, so
/// no filesystem mutation ever happens just to report a stat. The bytes are
/// `None` when the size cannot be read, so the status line shows an unknown
/// marker instead of hiding the field.
pub(super) fn collect_cache(cache_root: &Path, hashing: HashingMode) -> CacheData {
    let unknown = CacheData {
        prunable_bytes: None,
        prunable_partial: false,
    };
    let current_version =
        matches!(hashing, HashingMode::Normal).then(crate::materialize::cache::version_major);
    let report = match crate::materialize::cache::prune(
        cache_root,
        crate::materialize::cache::PruneMode::StaleOnly,
        hashing,
        current_version.as_deref(),
        true, // dry_run: must not delete anything just to report a stat
    ) {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(
                "cache stat: cannot scan {} for prunable entries, so the size shows as unknown: {e:#}",
                cache_root.display()
            );
            return unknown;
        }
    };
    // An entry the scan could not read would make the total too small, so the total is unknown.
    if let Some((first, _)) = report.failed.first_key_value() {
        tracing::warn!(
            "cache stat: {} entries could not be read (first: {}), so the size shows as unknown",
            report.failed.len(),
            first.display()
        );
        return unknown;
    }
    let total = report
        .removed
        .iter()
        .try_fold(DirSize::default(), |total, p| {
            dir_size(p).map(|s| total.combine(s))
        });
    match total {
        Ok(total) => CacheData {
            prunable_bytes: Some(total.bytes),
            prunable_partial: total.skipped > 0,
        },
        Err(e) => {
            tracing::warn!(
                "cache stat: the prunable size is unknown, so it shows as unknown: {e:#}"
            );
            unknown
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;

    #[test]
    fn collect_cache_missing_cache_root_yields_zero_prunable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let result = collect_cache(&missing, HashingMode::Strict);
        assert_eq!(
            result,
            CacheData {
                prunable_bytes: Some(0),
                prunable_partial: false,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_cache_reports_unknown_bytes_when_a_stale_folder_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("0.0.1-old-deadbeef");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("file.txt"), b"0123456789").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = collect_cache(dir.path(), HashingMode::Strict);

        // Restore access so the tempdir can be removed, whatever the assertion said.
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            result,
            CacheData {
                prunable_bytes: None,
                prunable_partial: false,
            }
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        /// Over any set of stale folders, the prunable size is the sum of their file sizes, and
        /// every folder is still on disk after the stat (#3064).
        #[test]
        fn collect_cache_sums_every_stale_folder_and_deletes_none(
            folders in proptest::collection::vec(
                proptest::collection::vec(0usize..2048, 0..4),
                0..5,
            ),
            normal in proptest::bool::ANY,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let hashing = if normal { HashingMode::Normal } else { HashingMode::Strict };
            if normal {
                // The current generation is never prunable, whatever it holds.
                let current = dir.path().join(crate::materialize::cache::version_major());
                std::fs::create_dir_all(&current).unwrap();
                std::fs::write(current.join("kept"), vec![b'x'; 300]).unwrap();
            }
            let mut total = 0u64;
            let mut paths = Vec::new();
            for (i, files) in folders.iter().enumerate() {
                let stale = dir.path().join(format!("0.0.1-old-{i:08x}"));
                std::fs::create_dir_all(stale.join("sub")).unwrap();
                for (j, len) in files.iter().enumerate() {
                    let parent = if j % 2 == 0 { stale.clone() } else { stale.join("sub") };
                    std::fs::write(parent.join(format!("f{j}")), vec![b'x'; *len]).unwrap();
                    total += u64::try_from(*len).unwrap();
                }
                paths.push(stale);
            }

            let result = collect_cache(dir.path(), hashing);

            proptest::prop_assert_eq!(
                result,
                CacheData { prunable_bytes: Some(total), prunable_partial: false }
            );
            for path in &paths {
                proptest::prop_assert!(path.exists(), "{} was deleted", path.display());
            }
        }
    }

    #[test]
    fn collect_cache_reports_prunable_bytes_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        // A stale folder (doesn't match the current VERSION_TAG prefix).
        let stale = dir.path().join("0.0.1-old-deadbeef");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("file.txt"), b"0123456789").unwrap(); // 10 bytes

        let result = collect_cache(dir.path(), HashingMode::Strict);
        assert_eq!(
            result,
            CacheData {
                prunable_bytes: Some(10),
                prunable_partial: false,
            }
        );
        // dry_run: the stale folder must still be on disk afterward.
        assert!(
            stale.exists(),
            "collect_cache must never delete anything just to report a stat"
        );
    }
}
