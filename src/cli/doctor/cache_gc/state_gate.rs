//! Decides whether the state sweeps can remove anything, so the lsof in-use list is fetched only
//! when a sweep might need it (#3039).

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::materialize::cbm_units::{Unit, scan_units_with};
use crate::materialize::tmp_sweep;

/// Whether a sweep of `state/tmp` or `state/codebase-memory` could remove an entry. Both sweeps
/// remove only entries older than `retention`, so `false` means the in-use list is not needed.
/// `cbm_live` says the codebase-memory folder is in use by config, which the sweep skips.
/// An entry that cannot be read counts as possibly stale. Every age is judged against `now`, the
/// clock of the sweep that follows, so the gate and the sweep agree (#3374).
pub(super) fn state_may_have_stale_entries(
    state_dir: &Path,
    retention: Duration,
    cbm_live: bool,
    source: crate::materialize::cache::AgeSource,
    now: SystemTime,
) -> bool {
    let tmp = state_dir.join("tmp");
    let tmp_may_be_stale = match tmp_sweep::stale_summary_in(
        &tmp,
        crate::materialize::expiry::AgeRule {
            older_than: retention,
            source,
        },
        &[],
        now,
    ) {
        Ok(summary) => summary.count > 0 || summary.unreadable > 0,
        Err(e) => {
            tracing::error!(
                "state GC cannot scan {}, so lsof runs: {e:#}. Fix: make the folder readable, then run the GC again",
                tmp.display()
            );
            true
        }
    };
    // A codebase-memory folder that a features.codebase_memory index_path uses is live, and the
    // sweep never removes it, so it needs no in-use list (#3075).
    tmp_may_be_stale
        || (!cbm_live
            && cbm_may_have_stale_units(&state_dir.join("codebase-memory"), retention, source, now))
}

/// Whether the codebase-memory sweep could act on `dir`: a unit past `retention`, or a unit whose
/// age cannot be read. It uses the sweep's own scan, so the gate and the sweep agree (#3072).
/// A unit that cannot be scanned counts as possibly stale, and it is logged.
fn cbm_may_have_stale_units(
    dir: &Path,
    retention: Duration,
    source: crate::materialize::cache::AgeSource,
    now: SystemTime,
) -> bool {
    match scan_units_with(dir, retention, source, now) {
        Ok(None) => false,
        Ok(Some(units)) => !units.iter().all(Unit::is_fresh),
        Err(e) => {
            tracing::error!(
                "state GC cannot scan {}, so lsof runs: {e:#}. Fix: make the folder readable, then \
                 run the GC again",
                dir.display()
            );
            true
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs::File;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::state_may_have_stale_entries;

    const DAY: Duration = Duration::from_secs(86_400);

    fn age(path: &Path) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - 30 * DAY)
            .unwrap();
    }

    #[test]
    fn a_state_folder_with_only_fresh_entries_needs_no_in_use_list() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("tmp")).unwrap();
        std::fs::write(state.path().join("tmp").join("fresh"), b"x").unwrap();
        std::fs::create_dir(state.path().join("codebase-memory")).unwrap();
        std::fs::write(state.path().join("codebase-memory").join("index.db"), b"x").unwrap();

        assert!(!state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }

    #[test]
    fn a_state_folder_with_no_tmp_and_no_index_needs_no_in_use_list() {
        let state = tempfile::tempdir().unwrap();

        assert!(!state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }

    #[test]
    fn a_live_codebase_memory_index_needs_no_in_use_list() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("codebase-memory")).unwrap();
        let db = state.path().join("codebase-memory").join("index.db");
        std::fs::write(&db, b"x").unwrap();
        age(&db);

        assert!(!state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            true,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }

    #[test]
    fn an_old_codebase_memory_database_needs_the_in_use_list() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("codebase-memory")).unwrap();
        let db = state.path().join("codebase-memory").join("index.db");
        std::fs::write(&db, b"x").unwrap();
        age(&db);

        assert!(state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }

    #[test]
    fn an_old_side_file_beside_a_fresh_database_needs_no_in_use_list() {
        // A database and its side files are one unit, judged by the newest file.
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("codebase-memory");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("index.db"), b"x").unwrap();
        let wal = dir.join("index.db-wal");
        std::fs::write(&wal, b"x").unwrap();
        age(&wal);

        assert!(!state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }

    #[test]
    fn an_old_index_log_needs_no_in_use_list_because_no_sweep_removes_it() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("codebase-memory");
        std::fs::create_dir(&dir).unwrap();
        let log = dir.join("index.log");
        std::fs::write(&log, b"x").unwrap();
        age(&log);

        assert!(!state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_codebase_memory_folder_needs_the_in_use_list() {
        use std::os::unix::fs::PermissionsExt as _;
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("codebase-memory");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let needs_lsof = state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now(),
        );

        // Restore the mode first, so the temporary folder can be removed.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(needs_lsof);
    }

    #[test]
    fn an_old_tmp_entry_needs_the_in_use_list() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("tmp")).unwrap();
        let old = state.path().join("tmp").join("old");
        std::fs::write(&old, b"x").unwrap();
        age(&old);

        assert!(state_may_have_stale_entries(
            state.path(),
            DAY * 7,
            false,
            crate::materialize::cache::AgeSource::MtimeOnly,
            SystemTime::now()
        ));
    }
}
