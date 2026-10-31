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
    lock_state_file_why(dir, key, label)
        .inspect_err(|why| tracing::error!("{label} lock not taken: {why}"))
        .ok()
}

/// [`lock_state_file`] with the reason for a lock that is not taken, for a caller that must name
/// the cause, such as a deny text.
///
/// # Errors
/// The `key` is not a plain name, the folder or the lock file cannot be made, or the lock stays
/// busy past about 200 ms.
pub(crate) fn lock_state_file_why(dir: &Path, key: &str, label: &str) -> Result<File, String> {
    lock_state_file_timed(dir, key, label).0
}

/// How long taking a state lock took (#3012).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LockTiming {
    /// Polls of the lock, so `1` means no contention. Zero when the lock file was never reached.
    pub attempts: u32,
    /// Time spent polling, in microseconds.
    pub wait_us: u64,
}

/// [`lock_state_file_why`] with the poll count and wait time, whether or not the lock was taken.
///
/// # Errors
/// As [`lock_state_file_why`].
pub(crate) fn lock_state_file_timed(
    dir: &Path,
    key: &str,
    label: &str,
) -> (Result<File, String>, LockTiming) {
    let mut timing = LockTiming::default();
    // The key can come from hook stdin; an unsafe value must never reach a path join.
    if !crate::paths::is_valid_short_name(key) {
        return (
            Err(format!("the key for {label} is not a plain name")),
            timing,
        );
    }
    if let Err(e) = crate::paths::create_dir_owner_only(dir) {
        return (Err(format!("cannot create {}: {e}", dir.display())), timing);
    }
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true).mode(0o600);
    let path = dir.join(format!("{key}.lock"));
    let file = match crate::paths::refuse_symlink(&mut opts).open(&path) {
        Ok(file) => file,
        Err(e) => return (Err(format!("cannot open {}: {e}", path.display())), timing),
    };
    let start = std::time::Instant::now();
    for _ in 0..LOCK_ATTEMPTS {
        timing.attempts += 1;
        match file.try_lock() {
            Ok(()) => {
                timing.wait_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                // The orphan prune goes by age, so a held lock must look new.
                if let Err(e) = file.set_modified(std::time::SystemTime::now()) {
                    tracing::error!("cannot refresh {label} lock age: {e}");
                }
                return (Ok(file), timing);
            }
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            Err(e) => {
                timing.wait_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                return (Err(format!("cannot lock {}: {e}", path.display())), timing);
            }
        }
    }
    timing.wait_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
    let why = format!(
        "{} stays locked by another hook of this session",
        path.display()
    );
    (Err(why), timing)
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
/// The age is the entry's own, not its target's: the caller removes the entry itself, so a
/// symlink must be judged by its own mtime (#3094, #3108).
fn file_age(path: &Path) -> std::io::Result<Option<Duration>> {
    let modified = match std::fs::symlink_metadata(path) {
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

/// The state file name for the agent that fired the hook. Parallel subagents share the session
/// id, so each subagent gets its own state (#2901, #2936). The main agent keeps the bare session
/// id, which the Stop handlers and the `/clear` reset also use. The agent part is a hash of the
/// session id and the agent id, so any `agent_id` is safe in a path and no two keys are equal.
/// A session id that is not a plain name comes back as it is, so the caller still rejects it.
pub(crate) fn agent_state_key(session_id: &str, payload: &serde_json::Value) -> String {
    let agent_id = match payload.get("agent_id") {
        None | Some(serde_json::Value::Null) => return session_id.to_string(),
        _ if !crate::paths::is_valid_short_name(session_id) => return session_id.to_string(),
        Some(serde_json::Value::String(id)) => id.clone(),
        Some(other) => other.to_string(),
    };
    let hash = super::record_hash::hash_prefix(format!("{session_id}\0{agent_id}").as_bytes());
    format!("{session_id}.agent-{hash}")
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

/// Whether the session file at `json` is gone. A dangling link still counts as present, since
/// the entry exists and belongs to its session. `exists()` follows the link and would say absent
/// (#3116). An unreadable entry counts as present, so the lock is kept and the cause is logged.
pub(super) fn json_entry_is_absent(json: &Path) -> bool {
    match std::fs::symlink_metadata(json) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            note_prune_failure("cannot read the session file, so the lock stays", json, &e);
            false
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
            && json_entry_is_absent(&path.with_extension("json"))
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
mod tests;
