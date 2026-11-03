//! Tests of how the walk judges one entry that another process removed (#3276).

use std::time::{Duration, SystemTime};

use super::{Scan, judge_entry};
use crate::materialize::cache::AgeSource;
use crate::materialize::expiry::AgeRule;

fn rule() -> AgeRule {
    AgeRule {
        older_than: Duration::from_secs(86_400),
        source: AgeSource::MtimeOnly,
    }
}

#[test]
fn an_entry_removed_during_the_walk_is_neither_stale_nor_failed() {
    let root = tempfile::tempdir().unwrap();
    let gone = root.path().join("removed-after-listing");
    let mut scan = Scan::default();
    judge_entry(&gone, true, rule(), SystemTime::now(), &mut scan);
    assert!(scan.stale.is_empty(), "a vanished entry was marked stale");
    assert!(
        scan.failed.is_empty(),
        "a vanished entry was counted as unreadable"
    );
    assert_eq!(scan.kept, 0);
}

#[test]
fn a_file_entry_removed_during_the_walk_is_neither_stale_nor_failed() {
    let root = tempfile::tempdir().unwrap();
    let gone = root.path().join("file-removed-after-listing");
    let mut scan = Scan::default();
    judge_entry(&gone, false, rule(), SystemTime::now(), &mut scan);
    assert!(scan.stale.is_empty());
    assert!(scan.failed.is_empty());
}
