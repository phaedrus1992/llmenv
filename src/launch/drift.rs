//! Config-drift watch for `launch` (#1286): a session-scoped comparison
//! against the config `launch` resolved at startup, independent of the
//! `SessionStart`-only, Claude-Code-only check `hook_run::should_check_stale`
//! already performs (that one predates `launch` and doesn't cover drift
//! *during* an active session). See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::launch::socket::NoticeSlot;

pub(crate) const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

const DRIFT_NOTICE: &str =
    "llmenv config changed since this session started; restart to pick up changes.";

/// Whether `current`'s content hash differs from the session's `baseline`.
fn has_drifted(baseline: &str, current: &str) -> bool {
    baseline != current
}

/// Recompute the current config's content hash the same way `run_check_stale`
/// does, reusing its manifest-building pipeline rather than a second
/// implementation. Returns `Ok(None)` when there's no content to
/// materialize (mirrors `run_check_stale`'s own "not drifted" case for an
/// empty config).
pub(crate) fn current_hash(config_path: &Path) -> anyhow::Result<Option<String>> {
    let config = crate::config::Config::load(config_path)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let firing = crate::bundle_select::firing_bundles(&config.bundle, &active, None);
    match crate::cli::build_manifest(&config, config_dir, &active, &firing, false)? {
        Some((manifest, _)) => Ok(Some(crate::materialize::cache::hash_manifest(&manifest)?)),
        None => Ok(None),
    }
}

/// Poll for config drift every `interval` and queue a notice the first time
/// it's detected. Runs until the caller drops/aborts this task. Fail-soft:
/// an error recomputing the hash is logged and retried next interval, never
/// surfaced as a session failure.
pub(crate) async fn watch(
    baseline_hash: String,
    config_path: PathBuf,
    notices: NoticeSlot,
    interval: Duration,
) {
    let mut interval = tokio::time::interval(interval);
    let mut already_notified = false;
    loop {
        interval.tick().await;
        if already_notified {
            continue;
        }
        let path = config_path.clone();
        let current = match tokio::task::spawn_blocking(move || current_hash(&path)).await {
            Ok(Ok(hash)) => hash,
            Ok(Err(e)) => {
                tracing::debug!("launch: drift check failed: {e:#}");
                continue;
            }
            Err(e) => {
                tracing::debug!("launch: drift check task panicked: {e:#}");
                continue;
            }
        };
        let Some(current) = current else { continue };
        if has_drifted(&baseline_hash, &current) {
            *notices.lock().await = Some(DRIFT_NOTICE.to_string());
            already_notified = true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn has_drifted_is_true_when_current_hash_differs_from_baseline() {
        assert!(has_drifted("abc", "def"));
    }

    #[test]
    fn has_drifted_is_false_when_current_hash_matches_baseline() {
        assert!(!has_drifted("abc", "abc"));
    }

    /// A real, materializable config: a `user` scope matched to the test
    /// process's own OS user (so `scope::matcher::Env::detect()` picks it up
    /// for real, mirroring `tests/launch.rs::config_base`), tagged so a
    /// bundle with on-disk content actually fires — `build_manifest` returns
    /// `None` for a config with no firing bundle, which `current_hash`
    /// otherwise can't distinguish from "unchanged".
    fn drifting_config(cache_dir: &std::path::Path) -> String {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "runner".to_string());
        format!(
            r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

bundle:
  - name: t
    when: [test]

cache:
  cache_dir: "{cache_dir}"
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
            cache_dir = cache_dir.display(),
        )
    }

    #[tokio::test]
    async fn watch_queues_a_notice_once_the_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let bundle_dir = config_dir.join("bundles").join("t");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(bundle_dir.join("AGENTS.md"), "hello").unwrap();

        let config_path = config_dir.join("config.yaml");
        std::fs::write(&config_path, drifting_config(&dir.path().join("cache"))).unwrap();
        let baseline = current_hash(&config_path)
            .unwrap()
            .expect("this config has a firing bundle and must materialize");

        let notices: NoticeSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let handle = tokio::spawn(watch(
            baseline,
            config_path.clone(),
            std::sync::Arc::clone(&notices),
            Duration::from_millis(20),
        ));

        // No change yet: nothing queued after a couple of ticks.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(*notices.lock().await, None);

        // Same shape (same firing bundle), but its content changed.
        std::fs::write(bundle_dir.join("AGENTS.md"), "hello, world").unwrap();
        crate::launch::wait_for_notice(&notices).await;
        assert_eq!(
            notices.lock().await.as_deref(),
            Some(DRIFT_NOTICE),
            "expected a drift notice to be queued after the config changed"
        );

        handle.abort();
    }
}
