//! The walk of `state/tmp`: it sorts every entry into stale, kept, or failed, and changes
//! nothing. The sweep and the summary both build on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

use super::claude_root::{is_split_root, scan_split_root};
use crate::materialize::cache::{
    AgeSource, file_stamp, is_expired, is_vanished, newest_file_mtime,
};
use crate::materialize::expiry::AgeRule;

/// One entry that the scan found old enough to remove, and not in use.
pub(super) struct Stale {
    pub(super) path: PathBuf,
    pub(super) is_dir: bool,
}

/// What a scan of `state/tmp` found. Nothing is removed by a scan.
#[derive(Default)]
pub(super) struct Scan {
    pub(super) stale: Vec<Stale>,
    pub(super) kept: usize,
    /// Each entry that could not be read, with the reason, so `llmenv doctor` can name it.
    pub(super) failed: BTreeMap<PathBuf, String>,
}

/// Walk `tmp_root` and sort every entry into stale, kept, or failed, judged against `now`. The
/// walk changes nothing.
pub(super) fn scan(
    tmp_root: &Path,
    rule: AgeRule,
    in_use: &[PathBuf],
    now: SystemTime,
) -> anyhow::Result<Scan> {
    let mut scan = Scan::default();
    let Some(entries) = crate::paths::read_dir_optional(tmp_root)? else {
        return Ok(scan);
    };
    let root = std::fs::canonicalize(tmp_root)
        .with_context(|| format!("resolving {}", tmp_root.display()))?;
    for entry in entries {
        let Some(entry) = entry_or_failure(&root, entry, &mut scan) else {
            continue;
        };
        let name = entry.file_name();
        let path = root.join(&name);
        if in_use.iter().any(|used| used.starts_with(&path)) {
            scan.kept += 1;
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            // Another process removed the entry after the listing (#2835, #3423).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                record_failure(&path, &e, &mut scan);
                continue;
            }
        };
        // A root that holds unrelated entries is not judged as one entry, because one fresh entry
        // would keep all the others (#1379, #3184). Its entries are judged one by one instead.
        if is_dir && is_split_root(&name) {
            scan_split_root(&path, rule, in_use, now, &mut scan);
            continue;
        }
        judge_entry(&path, is_dir, rule, now, &mut scan);
    }
    Ok(scan)
}

/// Mark one entry stale when its newest file mtime is older than `older_than`. Keep it otherwise.
pub(super) fn judge_entry(
    path: &Path,
    is_dir: bool,
    rule: AgeRule,
    now: SystemTime,
    scan: &mut Scan,
) {
    let modified = match entry_mtime(path, is_dir, rule.source) {
        Ok(modified) => modified,
        // Another process removed the entry after the walk listed it. That is not a failure,
        // the same as a child that vanishes inside a folder walk (#2835, #3276).
        Err(e) if is_vanished(&e) => {
            tracing::debug!(path = %path.display(), "state/tmp entry removed during the walk");
            return;
        }
        Err(e) => {
            record_failure(path, &e, scan);
            return;
        }
    };
    // `is_expired` keeps a future mtime and logs the skew. A removal on a bad clock is not
    // reversible, so the entry must reach that check.
    if !is_expired(now, modified, rule.older_than, path) {
        scan.kept += 1;
    } else {
        scan.stale.push(Stale {
            path: path.to_path_buf(),
            is_dir,
        });
    }
}

/// The newest file mtime of an entry. A symlink is judged by its own mtime and never followed.
fn entry_mtime(path: &Path, is_dir: bool, source: AgeSource) -> anyhow::Result<SystemTime> {
    if is_dir {
        return newest_file_mtime(path, source);
    }
    std::fs::symlink_metadata(path)
        .and_then(|meta| file_stamp(&meta, source))
        .with_context(|| format!("stat {}", path.display()))
}

/// Keep a readable entry of `root`. The read error does not carry the entry name, so the record
/// names the folder and the cause.
fn entry_or_failure(
    root: &Path,
    entry: std::io::Result<std::fs::DirEntry>,
    scan: &mut Scan,
) -> Option<std::fs::DirEntry> {
    match entry {
        Ok(entry) => Some(entry),
        Err(e) => {
            record_failure(root, format!("an entry in {} ({e})", root.display()), scan);
            None
        }
    }
}

/// Record an entry the sweep could not read or judge. The entry stays, and the next run retries.
pub(super) fn record_failure(path: &Path, error: impl std::fmt::Display, scan: &mut Scan) {
    tracing::error!(
        path = %path.display(),
        error = %error,
        "state/tmp sweep: cannot read entry; skipping"
    );
    scan.failed.insert(path.to_path_buf(), error.to_string());
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: a failed setup step should abort the test"
)]
mod tests {
    use std::fs::File;

    use super::*;
    use crate::materialize::tmp_sweep::tests::DAY;
    use crate::test_log_capture::capture_logs;

    /// A missing entry is recognised at every depth of context, and no other kind is.
    /// The domain is finite, so every pair is checked.
    #[test]
    fn a_not_found_is_vanished_at_every_context_depth() {
        use std::io::ErrorKind;
        let kinds = [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::Other,
        ];
        for kind in kinds {
            for depth in 0..5 {
                let mut error = anyhow::Error::from(std::io::Error::from(kind));
                for layer in 0..depth {
                    error = error.context(format!("layer {layer}"));
                }
                assert_eq!(
                    is_vanished(&error),
                    kind == ErrorKind::NotFound,
                    "{kind:?} at {depth}"
                );
            }
        }
    }

    proptest::proptest! {
        /// Every entry of the folder lands in exactly one class: stale, kept, or failed.
        #[test]
        fn every_entry_is_classified_exactly_once(ages in proptest::collection::vec(0u64..30, 0..8)) {
            use crate::materialize::tmp_sweep::tests::mtime_rule;
            let dir = tempfile::tempdir().unwrap();
            let now = std::time::SystemTime::now();
            for (i, age_days) in ages.iter().enumerate() {
                let file = dir.path().join(format!("entry-{i}.tmp"));
                std::fs::write(&file, "x").unwrap();
                let mtime = now - std::time::Duration::from_secs(age_days * 86_400);
                filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();
            }
            let rule = mtime_rule(std::time::Duration::from_secs(7 * 86_400));
            let result = scan(dir.path(), rule, &[], SystemTime::now()).unwrap();
            proptest::prop_assert_eq!(result.stale.len() + result.kept + result.failed.len(), ages.len());
        }

        /// An entry is stale only when it is past the threshold and no in-use path is at or under
        /// it. An entry that holds an in-use path is never stale, whatever its age (#3280).
        #[test]
        fn an_entry_is_stale_only_when_old_and_not_in_use(
            entries in proptest::collection::vec(
                // Seven days is the threshold. It is left out, so the expected verdict is clear.
                (proptest::strategy::Strategy::prop_map(0u64..30, |d| if d == 7 { 8 } else { d }), proptest::option::of(proptest::bool::ANY)),
                0..6,
            ),
        ) {
            use crate::materialize::tmp_sweep::tests::mtime_rule;
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let now = SystemTime::now();
            let mut in_use = Vec::new();
            let mut expected = std::collections::BTreeSet::new();
            for (i, (age_days, used)) in entries.iter().enumerate() {
                let entry = root.join(format!("e{i}"));
                let child = entry.join("f");
                std::fs::create_dir(&entry).unwrap();
                std::fs::write(&child, b"x").unwrap();
                let mtime = now - DAY * u32::try_from(*age_days).unwrap();
                filetime::set_file_mtime(&child, filetime::FileTime::from_system_time(mtime)).unwrap();
                match used {
                    Some(true) => in_use.push(child),
                    Some(false) => in_use.push(entry.clone()),
                    None if *age_days > 7 => {
                        expected.insert(entry);
                    }
                    None => {}
                }
            }
            let result = scan(&root, mtime_rule(DAY * 7), &in_use, now).unwrap();
            let stale: std::collections::BTreeSet<PathBuf> =
                result.stale.into_iter().map(|s| s.path).collect();
            proptest::prop_assert!(result.failed.is_empty(), "{:?}", result.failed);
            proptest::prop_assert_eq!(result.kept + stale.len(), entries.len());
            proptest::prop_assert_eq!(stale, expected);
        }
    }

    #[test]
    fn a_future_mtime_entry_is_kept_and_its_clock_skew_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("future.txt");
        std::fs::write(&file, b"f").unwrap();
        File::open(&file)
            .unwrap()
            .set_modified(SystemTime::now() + DAY)
            .unwrap();

        let mut scan = Scan::default();
        let logs = capture_logs(|| {
            judge_entry(
                &file,
                false,
                AgeRule {
                    older_than: DAY * 7,
                    source: AgeSource::MtimeOnly,
                },
                SystemTime::now(),
                &mut scan,
            );
        });

        assert!(scan.stale.is_empty(), "a future mtime must not be stale");
        assert_eq!(scan.kept, 1);
        assert!(logs.contains("clock skew"), "skew not logged: {logs}");
    }

    #[test]
    fn an_unreadable_entry_is_logged_against_its_folder_and_skipped() {
        let root = Path::new("/state/tmp");
        let mut scan = Scan::default();
        let error = std::io::Error::other("bad directory block");
        let mut entry = None;
        let logs = capture_logs(|| {
            entry = entry_or_failure(root, Err(error), &mut scan);
        });
        assert!(entry.is_none());
        assert_eq!(
            scan.failed.keys().cloned().collect::<Vec<PathBuf>>(),
            vec![root.to_path_buf()]
        );
        assert!(logs.contains("an entry in /state/tmp"), "{logs}");
        assert!(logs.contains("bad directory block"), "{logs}");
    }
}
