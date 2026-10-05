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
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
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
/// a best-effort cleanup, not correctness-critical.
pub(crate) fn prune_stale_json_files(dir: &Path, max_age_days: u64) {
    let max_age_secs = max_age_days * 86_400;
    let now = unix_now();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!(
                "llmenv: failed to read {} for stale-session pruning: {e}",
                dir.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&path).inspect_err(|e| {
            tracing::warn!(
                "prune_stale_json_files: stat failed for {}: {e}",
                path.display()
            )
        }) && let Ok(modified) = meta.modified().inspect_err(|e| {
            tracing::warn!(
                "prune_stale_json_files: mtime failed for {}: {e}",
                path.display()
            )
        }) && let Ok(duration) =
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .inspect_err(|e| {
                    tracing::warn!(
                        "prune_stale_json_files: duration_since failed for {}: {e}",
                        path.display()
                    )
                })
        {
            let age_secs = now.saturating_sub(duration.as_secs() as i64);
            if age_secs > max_age_secs as i64
                && let Err(e) = std::fs::remove_file(&path)
            {
                eprintln!(
                    "llmenv: failed to prune stale state file {}: {e}",
                    path.display()
                );
            }
        }
    }
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

/// Remove `.lock` files in `dir` older than `max_age_days` whose `.json` is gone. A session in its
/// first update holds a lock with no `.json` yet; removing that lock would let a second writer
/// lock a new file and lose an update, so only old locks go.
pub(crate) fn prune_orphan_locks(dir: &Path, max_age_days: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let max_age = Duration::from_secs(max_age_days * 86_400);
    for path in entries.flatten().map(|e| e.path()) {
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale
            && path.extension().and_then(|e| e.to_str()) == Some("lock")
            && !path.with_extension("json").exists()
        {
            let _ignored = std::fs::remove_file(&path);
        }
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
