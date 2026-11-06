//! Groups the index databases under `state/codebase-memory` into units and reads each unit's age
//! (#2817). The sweep removes the expired units, and the state GC gate asks whether any unit could
//! be removed. Both use this one scan, so they agree on what is removable (#3072).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;

use crate::materialize::cache::{AgeSource, file_stamp, is_expired};

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

/// How old one unit is, compared with the retention.
#[derive(Debug)]
enum Age {
    /// The newest file is past the retention, so the unit may be removed.
    Expired,
    /// The newest file is within the retention, so the unit is kept.
    Fresh,
    /// The age of this file cannot be read. The unit is kept, and it is reported as failed.
    Unreadable(PathBuf, std::io::Error),
}

/// One index database and its side files. The unit is judged as a whole.
#[derive(Debug)]
pub(crate) struct Unit {
    pub(crate) files: Vec<PathBuf>,
    age: Age,
}

impl Unit {
    /// Whether the newest file is past the retention.
    pub(crate) fn is_expired(&self) -> bool {
        matches!(self.age, Age::Expired)
    }

    /// Whether the newest file is within the retention.
    pub(crate) fn is_fresh(&self) -> bool {
        matches!(self.age, Age::Fresh)
    }

    /// The file whose age cannot be read, and its error, or `None` when every age was read.
    pub(crate) fn unreadable(&self) -> Option<(&Path, &std::io::Error)> {
        match &self.age {
            Age::Unreadable(file, e) => Some((file.as_path(), e)),
            Age::Expired | Age::Fresh => None,
        }
    }
}

/// Group the index databases in `dir` into units, and read the age of each unit once. `None`
/// means `dir` does not exist.
///
/// # Errors
/// `dir` exists but cannot be read, or one of its entries cannot be read.
/// Production passes `AgeSource::MtimeOrCtime`, so a restored index keeps its old mtime and the
/// change time counts too (#3275).
///
/// Each unit is judged against `now`, the caller's clock (#3374).
pub(crate) fn scan_units_with(
    dir: &Path,
    older_than: Duration,
    source: AgeSource,
    now: SystemTime,
) -> anyhow::Result<Option<Vec<Unit>>> {
    let Some(entries) = crate::paths::read_dir_optional(dir)? else {
        return Ok(None);
    };
    let mut databases: BTreeMap<Vec<u8>, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries {
        // An unread entry can be the `-wal` of a database that is still in use, so no unit can be
        // judged complete. The scan stops, and the caller removes nothing.
        let entry = entry.with_context(|| {
            format!(
                "cannot read an entry of {}, so no index database was removed. Fix: check the \
                 disk or mount that holds the folder, then run the GC again",
                dir.display()
            )
        })?;
        if let Some(base) = database_of(&entry.file_name()) {
            databases.entry(base).or_default().push(entry.path());
        }
    }
    let units = databases
        .into_values()
        .filter_map(|files| unit_of(files, now, older_than, dir, source))
        .collect();
    Ok(Some(units))
}

/// Judge one unit. `None` when every file of the unit has already gone.
fn unit_of(
    files: Vec<PathBuf>,
    now: SystemTime,
    older_than: Duration,
    dir: &Path,
    source: AgeSource,
) -> Option<Unit> {
    let present = match present_mtimes(&files, source) {
        Ok(present) => present,
        Err((file, e)) => {
            return Some(Unit {
                files,
                age: Age::Unreadable(file, e),
            });
        }
    };
    if present.is_empty() {
        return None;
    }
    let newest = present
        .iter()
        .fold(SystemTime::UNIX_EPOCH, |newest, (_, modified)| {
            newest.max(*modified)
        });
    let age = if is_expired(now, newest, older_than, dir) {
        Age::Expired
    } else {
        Age::Fresh
    };
    let files = present.into_iter().map(|(file, _)| file).collect();
    Some(Unit { files, age })
}

/// The mtime of each file that still exists, read without following a symlink. A file that is
/// already gone is left out: SQLite can remove a `-wal` between the directory read and this stat.
fn present_mtimes(
    files: &[PathBuf],
    source: AgeSource,
) -> Result<Vec<(PathBuf, SystemTime)>, (PathBuf, std::io::Error)> {
    let mut present = Vec::with_capacity(files.len());
    for file in files {
        match std::fs::symlink_metadata(file).and_then(|meta| file_stamp(&meta, source)) {
            Ok(modified) => present.push((file.clone(), modified)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err((file.clone(), e)),
        }
    }
    Ok(present)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod restored_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::time::UNIX_EPOCH;

    use super::*;

    proptest::proptest! {
        /// A unit expires exactly when its newest present file is older than the retention. A file
        /// that has gone is left out of the unit.
        #[test]
        fn a_unit_expires_on_its_newest_present_file(
            ages in proptest::collection::vec(0u64..864_000, 1..4),
            older_days in 1u64..8,
        ) {
            let dir = tempfile::tempdir().unwrap();
            // Whole seconds, so each mtime is exact and the boundary is not blurred by rounding.
            let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
            let mut files = Vec::new();
            for (i, age) in ages.iter().enumerate() {
                let file = dir.path().join(format!("repo-{i}.db"));
                std::fs::write(&file, "x").unwrap();
                let mtime = now - Duration::from_secs(*age);
                filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();
                files.push(file);
            }
            let gone = dir.path().join("repo-gone.db-wal");
            files.push(gone.clone());
            let older_than = Duration::from_secs(older_days * 86_400);

            let unit = unit_of(files, now, older_than, dir.path(), AgeSource::MtimeOnly).unwrap();

            let newest_age = Duration::from_secs(*ages.iter().min().unwrap());
            proptest::prop_assert_eq!(matches!(unit.age, Age::Expired), newest_age > older_than);
            proptest::prop_assert!(!unit.files.contains(&gone));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_database_name_is_judged() {
        use std::os::unix::ffi::OsStrExt as _;
        let name = OsStr::from_bytes(b"repo-\xff.db-shm");
        let base = database_of(name).unwrap();
        assert_eq!(base, b"repo-\xff");
        assert!(database_of(OsStr::new("index.log")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn database_of_strips_exactly_the_three_database_suffixes_for_any_bytes() {
        use proptest::prelude::*;
        use std::os::unix::ffi::OsStrExt as _;
        const ENDINGS: [&[u8]; 4] = [b".db", b".db-wal", b".db-shm", b".log"];
        proptest!(|(base in proptest::collection::vec(any::<u8>(), 0..24), ending in 0usize..4)| {
            let name = [base.as_slice(), ENDINGS[ending]].concat();
            let got = database_of(OsStr::from_bytes(&name));
            if ending < 3 {
                prop_assert_eq!(got, Some(base));
            } else {
                prop_assert!(got.is_none());
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn units_hold_each_database_file_once_and_only_database_files() {
        use proptest::prelude::*;
        const NAMES: [&str; 8] = [
            "a.db", "a.db-wal", "a.db-shm", "b.db", "b.db-wal", "a.log", "c.db.db", "d",
        ];
        proptest!(ProptestConfig::with_cases(24), |(picks in proptest::collection::vec(0usize..NAMES.len(), 0..12))| {
            let dir = tempfile::tempdir().unwrap();
            let written: BTreeSet<&str> = picks.iter().map(|i| NAMES[*i]).collect();
            for name in &written {
                std::fs::write(dir.path().join(name), "x").unwrap();
            }
            let units = scan_units_with(dir.path(), Duration::from_secs(3600), AgeSource::MtimeOnly, SystemTime::now()).unwrap().unwrap();

            let mut grouped: Vec<String> = units
                .iter()
                .flat_map(|unit| unit.files.iter())
                .map(|file| file.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            grouped.sort();
            let expected: Vec<String> = written
                .iter()
                .filter(|name| database_of(OsStr::new(name)).is_some())
                .map(|name| (*name).to_owned())
                .collect();
            prop_assert_eq!(grouped, expected);
            for unit in &units {
                let bases: BTreeSet<Vec<u8>> = unit
                    .files
                    .iter()
                    .map(|file| database_of(file.file_name().unwrap()).unwrap())
                    .collect();
                prop_assert_eq!(bases.len(), 1);
            }
        });
    }

    #[test]
    fn a_file_that_is_already_gone_is_left_out_of_the_judgement() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("a.db-wal");

        let present = present_mtimes(std::slice::from_ref(&gone), AgeSource::MtimeOnly).unwrap();

        assert!(present.is_empty());
    }
}
