//! Size rotation of the session-log file, and the prune of its rotated files (#2816).
//!
//! The active file rotates when it reaches [`MAX_BYTES`]. A rotation shifts the older files up
//! one generation, so `<name>.1` holds the newest rotated lines.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::materialize::cache::is_expired;

/// Size at which the active file rotates. Each line is one hook event, so the cap bounds how
/// much busy hooks add to one file (#2816).
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Rotated generations kept beside the active file. A rotation drops the oldest one.
pub(crate) const KEPT_ROTATIONS: u32 = 3;

/// Rotate `path` when it holds at least `max_bytes`. Returns the name of the first rotation,
/// or `None` when nothing rotated.
///
/// The rotation holds an advisory lock on a sibling lock file, and checks the length again
/// under it. Two processes can both see a full file. Without the lock, the second would move
/// the first one's new active file into generation 1 (#2831).
pub(crate) fn rotate_if_full(path: &Path, max_bytes: u64) -> std::io::Result<Option<String>> {
    if !is_full(path, max_bytes)? {
        return Ok(None);
    }
    let _lock = rotation_lock(path)?;
    if !is_full(path, max_bytes)? {
        return Ok(None);
    }
    ignore_missing(std::fs::remove_file(rotation_path(path, KEPT_ROTATIONS)))?;
    for n in (1..KEPT_ROTATIONS).rev() {
        ignore_missing(std::fs::rename(
            rotation_path(path, n),
            rotation_path(path, n + 1),
        ))?;
    }
    let first = rotation_path(path, 1);
    std::fs::rename(path, &first)?;
    Ok(first
        .file_name()
        .map(|name| name.to_string_lossy().into_owned()))
}

/// Whether `path` holds at least `max_bytes`. A missing file is not full.
fn is_full(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.len() >= max_bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The suffix of the rotation lock file beside the active log.
const LOCK_SUFFIX: &str = ".rotate.lock";

/// A hook must not stall on a stuck rotation, so the lock gives up after about 500 ms. The
/// append then goes to the active file, and the next append tries the rotation again.
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Take the rotation lock beside `path`. A rotation holds it only for a few renames, so a short
/// bounded wait is enough.
///
/// # Errors
/// The lock file cannot be opened, or the lock stays busy for the whole wait.
fn rotation_lock(path: &Path) -> std::io::Result<std::fs::File> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(LOCK_SUFFIX);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = crate::paths::refuse_symlink(&mut opts).open(path.with_file_name(name))?;
    for _ in 0..LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "the session log rotation lock beside {} stays busy",
            path.display()
        ),
    ))
}

/// The first line of a file that follows a rotation. It names the file the old lines moved to.
pub(crate) fn rotation_marker(rotated_to: &str) -> String {
    serde_json::json!({"llmenv_log": "rotated", "rotated_to": rotated_to}).to_string()
}

/// The path of rotation `n` of `path`, such as `session-log.jsonl.1`.
pub(crate) fn rotation_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Treat a file that is already gone as done. Any other failure is returned.
fn ignore_missing(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Remove the rotated files of `path` that were last written more than `older_than` ago. The
/// active file is never removed. Returns how many files were removed (#2816).
///
/// A file that cannot be removed does not stop the pass. The error names every file that failed,
/// with the count of files that were removed.
///
/// # Errors
/// Fails when the directory cannot be read, or when any rotated file could not be removed.
pub(crate) fn prune_rotations(path: &Path, older_than: Duration) -> anyhow::Result<usize> {
    let (Some(dir), Some(base)) = (path.parent(), path.file_name()) else {
        return Ok(0);
    };
    let Some(entries) = crate::paths::read_dir_optional(dir)? else {
        return Ok(0);
    };
    let now = SystemTime::now();
    let mut removed = 0;
    let mut failures = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failures.push(format!("an entry in {}: {e}", dir.display()));
                continue;
            }
        };
        if !is_rotation_of(&entry.file_name(), base) {
            continue;
        }
        let entry_path = entry.path();
        let modified = match entry.metadata().and_then(|meta| meta.modified()) {
            Ok(modified) => modified,
            Err(e) => {
                failures.push(format!("{}: {e}", entry_path.display()));
                continue;
            }
        };
        if !is_expired(now, modified, older_than) {
            continue;
        }
        match std::fs::remove_file(&entry_path) {
            Ok(()) => removed += 1,
            Err(e) => failures.push(format!("{}: {e}", entry_path.display())),
        }
    }
    if !failures.is_empty() {
        anyhow::bail!(
            "{} rotated session-log file(s) could not be checked or removed, {removed} removed: {}. \
             Fix: check the permissions of the state directory",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(removed)
}

/// Whether `name` is `<base>.<digits>`, the name of a rotated file.
fn is_rotation_of(name: &std::ffi::OsStr, base: &std::ffi::OsStr) -> bool {
    let (Some(name), Some(base)) = (name.to_str(), base.to_str()) else {
        return false;
    };
    name.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs::File;

    fn write_bytes(path: &Path, len: u64) {
        std::fs::write(path, vec![b'x'; usize::try_from(len).unwrap()]).unwrap();
    }

    #[test]
    fn a_rotation_that_waits_on_the_lock_checks_the_length_again() {
        // Another process rotated while this one waited, so the active file is small again.
        // A second rotation would move that new file into generation 1 (#2831).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "x".repeat(20)).unwrap();
        let held = rotation_lock(&path).unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || rotate_if_full(&path, 10))
        };
        // Give the waiter time to pass its first length check and block on the lock.
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::fs::rename(&path, rotation_path(&path, 1)).unwrap();
        std::fs::write(&path, "new").unwrap();
        drop(held);

        assert_eq!(waiter.join().unwrap().unwrap(), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!rotation_path(&path, 2).exists());
    }

    #[test]
    fn a_file_under_the_cap_is_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&path, 9);
        assert_eq!(rotate_if_full(&path, 10).unwrap(), None);
        assert!(path.exists());
        assert!(!rotation_path(&path, 1).exists());
    }

    #[test]
    fn a_file_at_the_cap_rotates_to_generation_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&path, 10);
        let rotated = rotate_if_full(&path, 10).unwrap();
        assert_eq!(rotated.as_deref(), Some("session-log.jsonl.1"));
        assert!(!path.exists(), "the active file must move away");
        assert_eq!(
            std::fs::metadata(rotation_path(&path, 1)).unwrap().len(),
            10
        );
    }

    #[test]
    fn rotations_keep_only_the_last_kept_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        let rounds = KEPT_ROTATIONS + 2;
        for round in 0..rounds {
            // Each round writes a distinct size, so the generations can be told apart.
            write_bytes(&path, 10 + u64::from(round));
            rotate_if_full(&path, 10).unwrap();
        }
        for n in 1..=KEPT_ROTATIONS {
            assert!(
                rotation_path(&path, n).exists(),
                "generation {n} is missing"
            );
        }
        assert!(!rotation_path(&path, KEPT_ROTATIONS + 1).exists());
        // Generation 1 holds the newest rotated file, the last round's.
        let last = 10 + u64::from(rounds - 1);
        assert_eq!(
            std::fs::metadata(rotation_path(&path, 1)).unwrap().len(),
            last
        );
    }

    #[test]
    fn a_missing_active_file_is_not_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        assert_eq!(rotate_if_full(&path, 10).unwrap(), None);
    }

    #[test]
    fn the_rotation_marker_names_the_file_the_old_lines_moved_to() {
        let marker = rotation_marker("session-log.jsonl.1");
        let value: serde_json::Value = serde_json::from_str(&marker).unwrap();
        assert_eq!(value["llmenv_log"], "rotated");
        assert_eq!(value["rotated_to"], "session-log.jsonl.1");
    }

    #[test]
    fn prune_removes_old_rotations_and_keeps_the_active_file_and_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&path, 3);
        let old_rotation = rotation_path(&path, 2);
        write_bytes(&old_rotation, 3);
        let old_time = SystemTime::now() - Duration::from_secs(30 * 86_400);
        File::options()
            .write(true)
            .open(&old_rotation)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        let unrelated = dir.path().join("session-log.jsonl.bak");
        write_bytes(&unrelated, 3);
        File::options()
            .write(true)
            .open(&unrelated)
            .unwrap()
            .set_modified(old_time)
            .unwrap();

        let removed = prune_rotations(&path, Duration::from_secs(7 * 86_400)).unwrap();

        assert_eq!(removed, 1);
        assert!(!old_rotation.exists());
        assert!(path.exists());
        assert!(
            unrelated.exists(),
            "a name without a numeric suffix is not a rotation"
        );
    }

    #[test]
    fn prune_keeps_a_recent_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        write_bytes(&rotation_path(&path, 1), 3);
        assert_eq!(
            prune_rotations(&path, Duration::from_secs(7 * 86_400)).unwrap(),
            0
        );
        assert!(rotation_path(&path, 1).exists());
    }

    #[test]
    fn prune_in_a_missing_directory_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("session-log.jsonl");
        assert_eq!(prune_rotations(&path, Duration::ZERO).unwrap(), 0);
    }

    #[test]
    fn the_rotation_marker_is_one_json_line_that_names_its_file() {
        use proptest::prelude::*;
        proptest!(|(name in "(?s).{0,40}")| {
            let marker = rotation_marker(&name);
            prop_assert!(!marker.contains('\n'), "marker spans lines: {marker}");
            let value: serde_json::Value = serde_json::from_str(&marker).unwrap();
            prop_assert_eq!(value["rotated_to"].as_str(), Some(name.as_str()));
        });
    }

    #[test]
    fn a_rotated_name_is_the_base_then_a_dot_then_digits() {
        use proptest::prelude::*;
        use std::ffi::OsStr;
        proptest!(|(digits in "[0-9]{1,6}", letter in "[a-z]{1,4}")| {
            let base = OsStr::new("session-log.jsonl");
            let rotated = format!("session-log.jsonl.{digits}");
            prop_assert!(is_rotation_of(OsStr::new(&rotated), base));
            let with_letters = format!("session-log.jsonl.{letter}{digits}");
            prop_assert!(!is_rotation_of(OsStr::new(&with_letters), base));
            let other_base = format!("other.jsonl.{digits}");
            prop_assert!(!is_rotation_of(OsStr::new(&other_base), base));
        });
    }

    #[test]
    fn the_rotation_lock_is_never_read_as_a_rotated_file() {
        use proptest::prelude::*;
        use std::ffi::OsStr;
        proptest!(|(base in "[a-z.-]{1,12}(\\.[0-9]{1,3})?")| {
            let lock = format!("{base}{LOCK_SUFFIX}");
            prop_assert!(!is_rotation_of(OsStr::new(&lock), OsStr::new(&base)));
        });
    }

    #[test]
    fn a_lock_held_past_the_wait_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        let _held = rotation_lock(&path).unwrap();
        let err = rotation_lock(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock, "{err}");
    }
}
