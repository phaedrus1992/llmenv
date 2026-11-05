//! Retention for the index databases under `state/codebase-memory` (#2817).
//!
//! llmenv passes `CBM_CACHE_DIR` to codebase-memory-mcp only when `index_path` is set. Without
//! it, the server keeps its indexes in its own default cache, and llmenv keeps only its index
//! log and index results in `state/codebase-memory`. A database file there is left from an
//! older setup that no server reads, so an old one can go. A folder that a configured
//! `index_path` names is the live cache, and it is never swept.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::materialize::cache::{GcReport, gc_remove_link};
use crate::materialize::cbm_units::scan_units_with;
use crate::materialize::expiry::AgeRule;

/// Remove the index databases in `dir` that `rule` expires at `now`. A
/// database and its `-wal` and `-shm` files are one unit: the newest of them gives the age, and
/// all go or none do. A database with a file in `in_use` is kept, because a server still has it
/// open. Other files, such as the index log and the index results, are kept. A file that cannot
/// be judged or removed is recorded in the report, and the sweep goes on.
///
/// With `dry_run`, the report lists the same files and nothing is removed (#2917).
///
/// # Errors
/// `dir` exists but cannot be read, or one of its entries cannot be read.
///
/// `rule.source` judges each file by mtime, or by the later of mtime and change time.
/// Production passes `AgeSource::MtimeOrCtime`, so a restored file is kept (#3275). `now` is the
/// caller's clock, so the gate and the sweep judge one time (#3374).
pub(crate) fn sweep_legacy_indexes(
    dir: &Path,
    rule: AgeRule,
    in_use: &[PathBuf],
    dry_run: bool,
    now: SystemTime,
) -> anyhow::Result<GcReport> {
    let mut report = GcReport::new(dry_run);
    let Some(units) = scan_units_with(dir, rule.older_than, rule.source, now)? else {
        return Ok(report);
    };
    for unit in units {
        // A failed age read is reported even for a unit that is in use, so no error is dropped.
        if let Some((file, e)) = unit.unreadable() {
            tracing::error!("cbm sweep cannot read the age of {}: {e}", file.display());
            let cause = format!("cannot read the age of {}: {e}", file.display());
            for path in unit.files {
                report.record_failure(path, cause.clone());
            }
        } else if unit
            .files
            .iter()
            .any(|f| in_use.iter().any(|used| used == f))
        {
            report.kept += unit.files.len();
        } else if unit.is_expired() {
            for file in &unit.files {
                gc_remove_link(file, &mut report);
            }
        } else {
            report.kept += unit.files.len();
        }
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::materialize::cache::AgeSource;
    use std::time::Duration;

    fn backdate(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
    }

    #[test]
    fn a_dry_run_lists_exactly_what_a_real_run_removes_and_keeps_units_whole() {
        use proptest::prelude::*;
        const SUFFIXES: [&str; 4] = [".db", ".db-wal", ".db-shm", ".log"];
        let file = (
            0usize..3,
            0usize..SUFFIXES.len(),
            any::<bool>(),
            any::<bool>(),
        );
        proptest!(ProptestConfig::with_cases(24), |(files in proptest::collection::vec(file, 0..10))| {
            let dir = tempfile::tempdir().unwrap();
            let mut in_use = Vec::new();
            for (base, suffix, is_old, held) in &files {
                let path = dir.path().join(format!("repo{base}{}", SUFFIXES[*suffix]));
                std::fs::write(&path, "x").unwrap();
                if *is_old {
                    backdate(&path);
                }
                if *held {
                    in_use.push(path);
                }
            }
            let week = Duration::from_secs(7 * 86_400);
            let names = |d: &Path| -> Vec<std::ffi::OsString> {
                let mut v: Vec<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name()).collect();
                v.sort();
                v
            };
            let before = names(dir.path());

            let preview = sweep_legacy_indexes(dir.path(), AgeRule { older_than: week, source: AgeSource::MtimeOnly }, &in_use, true, SystemTime::now()).unwrap();
            prop_assert_eq!(names(dir.path()), before.clone());
            let real = sweep_legacy_indexes(dir.path(), AgeRule { older_than: week, source: AgeSource::MtimeOnly }, &in_use, false, SystemTime::now()).unwrap();

            let mut listed = preview.removed.clone();
            listed.sort();
            let mut removed = real.removed.clone();
            removed.sort();
            prop_assert_eq!(&listed, &removed);
            // A database goes with all of its files or not at all.
            for base in 0..3 {
                let stem = format!("repo{base}.db");
                let unit: Vec<_> = before
                    .iter()
                    .filter(|n| n.to_string_lossy().starts_with(&stem))
                    .collect();
                let gone = unit.iter().filter(|n| !dir.path().join(n).exists()).count();
                prop_assert!(gone == 0 || gone == unit.len(), "unit {} split: {:?}", stem, unit);
            }
        });
    }

    #[test]
    fn a_dry_run_lists_the_old_database_files_and_removes_none() {
        let dir = tempfile::tempdir().unwrap();
        let old_db = dir.path().join("Users-x-repo.db");
        let old_wal = dir.path().join("Users-x-repo.db-wal");
        for p in [&old_db, &old_wal] {
            std::fs::write(p, "x").unwrap();
            backdate(p);
        }

        let week = Duration::from_secs(7 * 86_400);
        let report = sweep_legacy_indexes(
            dir.path(),
            AgeRule {
                older_than: week,
                source: AgeSource::MtimeOnly,
            },
            &[],
            true,
            SystemTime::now(),
        )
        .unwrap();

        let mut listed = report.removed.clone();
        listed.sort();
        assert_eq!(listed, vec![old_db.clone(), old_wal.clone()]);
        assert!(old_db.exists() && old_wal.exists());
    }

    #[test]
    fn only_old_database_files_go() {
        let dir = tempfile::tempdir().unwrap();
        let old_db = dir.path().join("Users-x-repo.db");
        let old_wal = dir.path().join("Users-x-repo.db-wal");
        let new_db = dir.path().join("Users-x-live.db");
        let log = dir.path().join("index.log");
        let result = dir.path().join("index-result-1.json");
        for p in [&old_db, &old_wal, &new_db, &log, &result] {
            std::fs::write(p, "x").unwrap();
        }
        for p in [&old_db, &old_wal, &log, &result] {
            backdate(p);
        }

        let week = Duration::from_secs(7 * 86_400);
        let report = sweep_legacy_indexes(
            dir.path(),
            AgeRule {
                older_than: week,
                source: AgeSource::MtimeOnly,
            },
            &[],
            false,
            SystemTime::now(),
        )
        .unwrap();

        assert_eq!(report.removed.len(), 2, "{report:?}");
        assert!(!old_db.exists() && !old_wal.exists());
        assert!(new_db.exists() && log.exists() && result.exists());
        assert_eq!(report.kept, 1);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn a_database_goes_whole_or_not_at_all_and_an_open_one_stays() {
        let dir = tempfile::tempdir().unwrap();
        // An old database with a fresh wal file is still written to, so all of it stays.
        let db = dir.path().join("a.db");
        let wal = dir.path().join("a.db-wal");
        // An old database that a server holds open stays too.
        let open = dir.path().join("b.db");
        for p in [&db, &wal, &open] {
            std::fs::write(p, "x").unwrap();
        }
        backdate(&db);
        backdate(&open);

        let week = Duration::from_secs(7 * 86_400);
        let report = sweep_legacy_indexes(
            dir.path(),
            AgeRule {
                older_than: week,
                source: AgeSource::MtimeOnly,
            },
            std::slice::from_ref(&open),
            false,
            SystemTime::now(),
        )
        .unwrap();

        assert!(report.removed.is_empty(), "{report:?}");
        assert!(db.exists() && wal.exists() && open.exists());
        assert_eq!(report.kept, 3);
    }

    #[test]
    fn a_missing_folder_is_an_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let report = sweep_legacy_indexes(
            &dir.path().join("absent"),
            AgeRule {
                older_than: Duration::ZERO,
                source: AgeSource::MtimeOnly,
            },
            &[],
            false,
            SystemTime::now(),
        )
        .unwrap();
        assert!(report.removed.is_empty() && report.failed.is_empty());
    }
}
