//! Tests of the entry-level error path of the `state/tmp` walk (#3291) and its property rules
//! (#3293).

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::*;
use crate::materialize::cache::AgeSource;
use crate::test_log_capture::capture_logs;

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

/// A missing entry is recognised at every depth of context, and no other kind is.
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
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for (i, age_days) in ages.iter().enumerate() {
            let file = dir.path().join(format!("entry-{i}.tmp"));
            std::fs::write(&file, "x").unwrap();
            let mtime = now - Duration::from_secs(age_days * 86_400);
            filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();
        }
        let rule = AgeRule {
            older_than: Duration::from_secs(7 * 86_400),
            source: AgeSource::MtimeOnly,
        };
        let result = scan(dir.path(), rule, &[]).unwrap();
        proptest::prop_assert_eq!(
            result.stale.len() + result.kept + result.failed.len(),
            ages.len()
        );
    }
}
