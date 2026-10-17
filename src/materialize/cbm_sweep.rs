//! Retention for the index databases under `state/codebase-memory` (#2817).
//!
//! llmenv passes `CBM_CACHE_DIR` to codebase-memory-mcp only when `index_path` is set. Without
//! it, the server keeps its indexes in its own default cache, and llmenv keeps only its index
//! log and index results in `state/codebase-memory`. A database file there is left from an
//! older setup that no server reads, so an old one can go. A folder that a configured
//! `index_path` names is the live cache, and it is never swept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::materialize::cache::{GcReport, gc_remove_link, is_expired};

/// The file name endings of a codebase-memory-mcp index database and its SQLite side files.
const DB_SUFFIXES: [&[u8]; 3] = [b".db", b".db-wal", b".db-shm"];

/// The name bytes of the database a file belongs to, or `None` for a file that is not an index
/// database. The name is compared as bytes, so a non-UTF-8 name is still judged.
fn database_of(name: &std::ffi::OsStr) -> Option<Vec<u8>> {
    let bytes = name.as_encoded_bytes();
    DB_SUFFIXES
        .iter()
        .find_map(|suffix| bytes.strip_suffix(*suffix))
        .map(<[u8]>::to_vec)
}

/// Remove the index databases in `dir` that were last written more than `older_than` ago. A
/// database and its `-wal` and `-shm` files are one unit: the newest of them gives the age, and
/// all go or none do. A database with a file in `in_use` is kept, because a server still has it
/// open. Other files, such as the index log and the index results, are kept. A file that cannot
/// be judged or removed is recorded in the report, and the sweep goes on.
///
/// # Errors
/// `dir` exists but cannot be read.
pub(crate) fn sweep_legacy_indexes(
    dir: &Path,
    older_than: Duration,
    in_use: &[PathBuf],
) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    let Some(entries) = crate::paths::read_dir_optional(dir)? else {
        return Ok(report);
    };
    let mut databases: BTreeMap<Vec<u8>, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries.filter_map(|entry| crate::paths::readable_entry(dir, "cbm sweep", entry)) {
        if let Some(base) = database_of(&entry.file_name()) {
            databases.entry(base).or_default().push(entry.path());
        }
    }
    let now = SystemTime::now();
    for files in databases.into_values() {
        if files.iter().any(|f| in_use.iter().any(|used| used == f)) {
            report.kept += files.len();
            continue;
        }
        match newest_of(&files) {
            Ok(newest) if is_expired(now, newest, older_than) => {
                for file in &files {
                    gc_remove_link(file, &mut report);
                }
            }
            Ok(_) => report.kept += files.len(),
            Err((file, e)) => {
                tracing::error!("cbm sweep cannot read the age of {}: {e}", file.display());
                report.failed.extend(files);
            }
        }
    }
    Ok(report)
}

/// The newest mtime of `files`, read without following a symlink.
fn newest_of(files: &[PathBuf]) -> Result<SystemTime, (PathBuf, std::io::Error)> {
    let mut newest = SystemTime::UNIX_EPOCH;
    for file in files {
        let modified = std::fs::symlink_metadata(file)
            .and_then(|meta| meta.modified())
            .map_err(|e| (file.clone(), e))?;
        newest = newest.max(modified);
    }
    Ok(newest)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn backdate(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
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
        let report = sweep_legacy_indexes(dir.path(), week, &[]).unwrap();

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
        let report = sweep_legacy_indexes(dir.path(), week, std::slice::from_ref(&open)).unwrap();

        assert!(report.removed.is_empty(), "{report:?}");
        assert!(db.exists() && wal.exists() && open.exists());
        assert_eq!(report.kept, 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_database_name_is_judged() {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::OsStr::from_bytes(b"repo-\xff.db-shm");
        let base = database_of(name).unwrap();
        assert_eq!(base, b"repo-\xff");
        assert!(database_of(std::ffi::OsStr::new("index.log")).is_none());
    }

    #[test]
    fn a_missing_folder_is_an_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let report = sweep_legacy_indexes(&dir.path().join("absent"), Duration::ZERO, &[]).unwrap();
        assert!(report.removed.is_empty() && report.failed.is_empty());
    }
}
