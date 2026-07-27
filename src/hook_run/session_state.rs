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

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
}
