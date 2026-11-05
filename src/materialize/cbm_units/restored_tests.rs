//! A codebase-memory index restored with an old mtime is not judged expired under the production
//! rule (#3275).

use std::time::{Duration, SystemTime};

use super::scan_units_with;
use crate::materialize::cache::AgeSource;

#[test]
fn a_database_restored_with_an_old_mtime_is_fresh_under_the_change_time_rule() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    std::fs::write(&db, "x").unwrap();
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(&db, filetime::FileTime::from_system_time(old)).unwrap();

    let units = scan_units_with(
        dir.path(),
        Duration::from_secs(86_400),
        AgeSource::MtimeOrCtime,
        SystemTime::now(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(units.len(), 1);
    assert!(
        units[0].is_fresh(),
        "a restored index was judged expired (#3275)"
    );
}
