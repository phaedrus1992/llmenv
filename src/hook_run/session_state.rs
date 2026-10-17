//! Shared helper for the per-session state files `read_once` and
//! `repeat_detect` each keep under `state_dir/<feature>/{session_id}.json`.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

/// A hook must not stall the agent on a busy state file, so a lock gives up after
/// `LOCK_ATTEMPTS` polls, about 200 ms.
const LOCK_ATTEMPTS: u32 = 20;
const LOCK_POLL: Duration = Duration::from_millis(10);

/// Lock `dir/<key>.lock`, creating the owner-only directory and the file. `label` names the
/// state in log lines. `None` for an unsafe `key`, an I/O error, or a lock that stays busy.
pub(crate) fn lock_state_file(dir: &Path, key: &str, label: &str) -> Option<File> {
    // The key can come from hook stdin; an unsafe value must never reach a path join.
    if !crate::paths::is_valid_short_name(key) {
        tracing::error!("key failed path-safety validation for {label}");
        return None;
    }
    if let Err(e) = crate::paths::create_dir_owner_only(dir) {
        tracing::error!("cannot create {}: {e}", dir.display());
        return None;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true).mode(0o600);
    let file = crate::paths::refuse_symlink(&mut opts)
        .open(dir.join(format!("{key}.lock")))
        .inspect_err(|e| tracing::error!("cannot open {label} lock: {e}"))
        .ok()?;
    for _ in 0..LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => {
                // The orphan prune goes by age, so a held lock must look new.
                if let Err(e) = file.set_modified(std::time::SystemTime::now()) {
                    tracing::error!("cannot refresh {label} lock age: {e}");
                }
                return Some(file);
            }
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            Err(e) => {
                tracing::error!("{label} lock failed, access skipped: {e}");
                return None;
            }
        }
    }
    tracing::error!("{label} busy, access skipped");
    None
}

/// Scan `dir` and delete `.json` files older than `max_age_days`. Fail-soft:
/// any stat/read error is logged and skipped, never propagated — pruning is
/// a best-effort cleanup, not correctness-critical. Each failure is also an agent
/// notice, because the engine drops a hook's stderr (#2746, #2833).
pub(crate) fn prune_stale_json_files(dir: &Path, max_age_days: u64) {
    let max_age = Duration::from_secs(max_age_days.saturating_mul(86_400));
    let Some(entries) = read_dir_logged(dir, "stale-session pruning") else {
        return;
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::error!(
                    "stale-session pruning skipped an entry in {}: {e}",
                    dir.display()
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match file_age(&path) {
            Ok(Some(age)) if is_stale(age, max_age) => {
                if let Some(e) = removal_error_to_log(std::fs::remove_file(&path)) {
                    note_prune_failure("cannot prune the stale state file", &path, &e);
                }
            }
            Ok(_) => {}
            Err(e) => note_prune_failure("cannot read the age of the state file", &path, &e),
        }
    }
}

/// Whether a file of `age` is past `max_age`. A file exactly at the limit is kept.
fn is_stale(age: Duration, max_age: Duration) -> bool {
    age > max_age
}

/// The age of `path`. `None` for a file that is gone, which is the normal outcome of a race.
/// An mtime in the future counts as age zero, so the file is kept.
fn file_age(path: &Path) -> std::io::Result<Option<Duration>> {
    let modified = match std::fs::metadata(path) {
        Ok(meta) => meta.modified()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(modified.elapsed().unwrap_or(Duration::ZERO)))
}

/// Logs a state-file pruning failure at error level, which the default filter shows, and
/// tells the agent.
fn note_prune_failure(what: &str, path: &Path, err: &std::io::Error) {
    crate::agent_notice::record_logged(format!(
        "llmenv: {what} {} ({err}), so it stays on disk. Fix: check the permissions of that \
         file and its folder, or run `llmenv doctor` to check the state directory.",
        path.display()
    ));
}

/// Whether a `SessionStart` `source` means the model's context was emptied, so
/// state that records what the model has seen no longer holds (#2381).
pub(crate) fn context_was_lost(source: Option<&str>) -> bool {
    matches!(source, Some("startup" | "clear" | "compact"))
}

/// Delete the read-once cache, the read-before-edit record, and the repeat-call
/// counter of one session.
/// After a compaction the model holds none of the file contents it read, so a
/// stale record would deny a re-read or excuse an edit of an unseen file.
/// The model also cannot remember the earlier calls, so a stale repeat counter
/// would warn about a call that is new to it.
/// Fail-soft: a file that cannot be removed is logged and the hook goes on.
pub(crate) fn reset_read_state(state_dir: &Path, session_id: &str) {
    if !crate::paths::is_valid_short_name(session_id) {
        tracing::debug!("reset_read_state: session id is not a valid state file name, skipped");
        return;
    }
    for path in [
        super::read_once::session_cache_path(state_dir, session_id),
        super::slippage::stats_path(state_dir, session_id),
        super::repeat_detect::session_state_path(state_dir, session_id),
    ] {
        if let Err(e) = remove_if_present(&path) {
            tracing::error!("reset_read_state: cannot remove {}: {e}", path.display());
        }
    }
}

/// Remove `path`; a file that is already gone counts as removed.
fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The error to log for a file removal, or `None` when the file is already gone. A missing file
/// is the normal outcome of a race, so it is not an error (#2657).
pub(crate) fn removal_error_to_log(result: std::io::Result<()>) -> Option<std::io::Error> {
    match result {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(e),
    }
}

/// The entries of `dir`, or `None` after a log line and an agent notice that name why they
/// cannot be read. A missing directory is normal, so it returns `None` with no log.
fn read_dir_logged(dir: &Path, purpose: &str) -> Option<std::fs::ReadDir> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::error!("cannot read {} for {purpose}: {e}", dir.display());
            crate::agent_notice::record(format!(
                "llmenv: {purpose} cannot read {} ({e}), so old state files stay on disk. Fix: \
                 check the permissions of that folder, or run `llmenv doctor`.",
                dir.display()
            ));
            None
        }
    }
}

/// Remove `.lock` files in `dir` older than `max_age_days` whose `.json` is gone. A session in its
/// first update holds a lock with no `.json` yet; removing that lock would let a second writer
/// lock a new file and lose an update, so only old locks go.
pub(crate) fn prune_orphan_locks(dir: &Path, max_age_days: u64) {
    let Some(entries) = read_dir_logged(dir, "stale lock pruning") else {
        return;
    };
    let max_age = Duration::from_secs(max_age_days.saturating_mul(86_400));
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                tracing::error!("lock pruning skipped an entry in {}: {e}", dir.display());
                continue;
            }
        };
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        // A lock whose age cannot be read is kept, and the cause is logged (#2858).
        let stale = match file_age(&path) {
            Ok(age) => age.is_some_and(|age| is_stale(age, max_age)),
            Err(e) => {
                note_prune_failure("cannot read the age of the lock", &path, &e);
                false
            }
        };
        if stale
            && !path.with_extension("json").exists()
            && let Some(e) = removal_error_to_log(std::fs::remove_file(&path))
        {
            note_prune_failure("cannot remove the stale lock", &path, &e);
        }
    }
}

#[cfg(test)]
mod removal_tests {
    use super::removal_error_to_log;
    use std::io::{Error, ErrorKind};

    #[test]
    fn a_removal_that_succeeds_or_finds_nothing_logs_nothing() {
        assert!(removal_error_to_log(Ok(())).is_none());
        assert!(removal_error_to_log(Err(Error::from(ErrorKind::NotFound))).is_none());
    }

    #[test]
    fn any_other_removal_error_is_returned_to_log() {
        let logged = removal_error_to_log(Err(Error::from(ErrorKind::PermissionDenied)));
        assert_eq!(logged.map(|e| e.kind()), Some(ErrorKind::PermissionDenied));
    }
}

/// Seconds since the Unix epoch; 0 for a clock set before 1970.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    #[test]
    fn prunes_only_stale_json_files() {
        let dir = TempDir::new().expect("test");
        let fresh = dir.path().join("fresh.json");
        let stale = dir.path().join("stale.json");
        let non_json = dir.path().join("ignored.txt");
        std::fs::write(&fresh, "{}").expect("test");
        std::fs::write(&stale, "{}").expect("test");
        std::fs::write(&non_json, "not json").expect("test");

        let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
        let old_ft = filetime::FileTime::from_system_time(old);
        filetime::set_file_mtime(&stale, old_ft).expect("test");

        prune_stale_json_files(dir.path(), 7);

        assert!(fresh.exists(), "fresh file must survive pruning");
        assert!(!stale.exists(), "stale file must be pruned");
        assert!(non_json.exists(), "non-json file must never be touched");
    }

    #[test]
    fn file_age_is_none_for_a_missing_file_and_an_error_for_an_unreadable_path() {
        let dir = TempDir::new().expect("test");
        assert!(file_age(&dir.path().join("absent.json")).unwrap().is_none());
        let file = dir.path().join("file");
        std::fs::write(&file, "x").expect("test");
        // A path under a regular file fails with ENOTDIR, not NotFound.
        assert!(file_age(&file.join("child.lock")).is_err());
        assert!(file_age(&file).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_whose_age_cannot_be_read_is_kept() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().expect("test");
        let locks = dir.path().join("locks");
        std::fs::create_dir(&locks).expect("test");
        let lock = locks.join("old.lock");
        std::fs::write(&lock, "").expect("test");
        let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
        filetime::set_file_mtime(&lock, filetime::FileTime::from_system_time(old)).expect("test");
        // Read without search permission: the folder lists, but a stat of its entries fails.
        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o444)).expect("test");

        let logs = crate::test_log_capture::capture_logs(|| prune_orphan_locks(&locks, 7));

        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o755)).expect("test");
        assert!(lock.exists(), "a lock whose age cannot be read must stay");
        assert!(
            logs.contains("old.lock") && logs.contains("stays on disk"),
            "the cause must be logged: {logs}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_prune_is_an_agent_notice() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().expect("test");
        let stale = dir.path().join("stale.json");
        std::fs::write(&stale, "{}").expect("test");
        let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old)).expect("test");
        // A read-only folder refuses the unlink.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).expect("test");

        prune_stale_json_files(dir.path(), 7);

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("test");
        assert!(stale.exists());
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains(&stale.display().to_string()) && notices.contains("Fix:"),
            "{notices}"
        );
    }

    #[test]
    fn a_file_at_the_limit_is_kept() {
        let day = Duration::from_secs(86_400);
        assert!(!is_stale(day, day));
        assert!(is_stale(day + Duration::from_secs(1), day));
        assert!(!is_stale(Duration::ZERO, day));
    }

    #[test]
    fn missing_dir_is_a_noop() {
        let dir = TempDir::new().expect("test");
        prune_stale_json_files(&dir.path().join("does-not-exist"), 7);
    }

    #[test]
    fn reset_read_state_removes_both_files_of_one_session_only() {
        let dir = TempDir::new().expect("test");
        let own = [
            super::super::read_once::session_cache_path(dir.path(), "s1"),
            super::super::slippage::stats_path(dir.path(), "s1"),
            super::super::repeat_detect::session_state_path(dir.path(), "s1"),
        ];
        let other = super::super::read_once::session_cache_path(dir.path(), "s2");
        for p in own.iter().chain([&other]) {
            std::fs::create_dir_all(p.parent().expect("test")).expect("test");
            std::fs::write(p, "{}").expect("test");
        }

        reset_read_state(dir.path(), "s1");

        assert!(own.iter().all(|p| !p.exists()), "own state must go");
        assert!(other.exists(), "another session's state must stay");
        reset_read_state(dir.path(), "s1");
        reset_read_state(dir.path(), "../escape");
    }

    #[test]
    fn remove_if_present_accepts_a_missing_file_and_reports_a_real_error() {
        let dir = TempDir::new().expect("test");
        let file = dir.path().join("f.json");
        remove_if_present(&file).unwrap();
        std::fs::write(&file, "{}").expect("test");
        remove_if_present(&file).unwrap();
        assert!(!file.exists());
        // `remove_file` fails on a directory, which is not a NotFound error.
        let sub = dir.path().join("d");
        std::fs::create_dir(&sub).expect("test");
        assert!(remove_if_present(&sub).is_err());
    }

    #[test]
    fn only_context_losing_sources_reset() {
        let got: Vec<bool> = ["startup", "clear", "compact", "resume", "fork"]
            .iter()
            .map(|s| context_was_lost(Some(s)))
            .collect();
        assert_eq!(got, [true, true, true, false, false]);
        assert!(!context_was_lost(None));
    }

    #[test]
    fn unix_now_reads_the_real_clock() {
        let expected = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test")
            .as_secs();
        let now = u64::try_from(unix_now()).expect("test");
        assert!(now.abs_diff(expected) <= 5, "{now} vs {expected}");
    }
}
