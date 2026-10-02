//! Shared helper for the per-session state files `read_once` and
//! `repeat_detect` each keep under `state_dir/<feature>/{session_id}.json`.

use std::path::Path;

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
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("reset_read_state: cannot remove {}: {e}", path.display()),
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
