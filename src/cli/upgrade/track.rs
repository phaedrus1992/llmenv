//! Picks the release track for `llmenv upgrade`: CLI flag, then config, then release.

use std::path::Path;

/// Resolve which release track to use: CLI flag > config > default (release).
pub(super) fn resolve_is_beta(track: Option<String>) -> bool {
    if let Some(t) = track {
        return t == "beta";
    }
    // Try `features.upgrade.track` from config
    match crate::paths::config_dir() {
        Ok(dir) => beta_track_from_config(&dir.join("config.yaml")),
        Err(e) => {
            tracing::warn!(
                "cannot find the config folder: {e:#}. Fix: set the config folder, or pass \
                 --track; the release track is used until then."
            );
            false
        }
    }
}

/// True when the config at `path` sets the beta track. A missing file is the normal no-config
/// case and stays silent. Any other read or parse failure is logged, because it changes the track.
fn beta_track_from_config(path: &Path) -> bool {
    match crate::config::Config::load(path) {
        Ok(cfg) => cfg
            .features
            .as_ref()
            .and_then(|f| f.upgrade.as_ref())
            .is_some_and(|upgrade| upgrade.track.as_str() == "beta"),
        Err(e) if is_missing_file(&e) => false,
        Err(e) => {
            tracing::warn!(
                "cannot read {}: {e:#}. Fix: correct the file, or pass --track; the release track \
                 is used until then.",
                path.display()
            );
            false
        }
    }
}

/// True when the error chain holds a `NotFound` I/O error.
fn is_missing_file(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test: a failed setup must stop it")]
mod tests {
    use super::*;
    use crate::test_log_capture::capture_logs;

    fn write_config(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("config.yaml");
        std::fs::write(&path, body).expect("write a test config");
        path
    }

    #[test]
    fn a_missing_config_is_release_and_logs_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.yaml");
        let mut got = true;
        let logs = capture_logs(|| got = beta_track_from_config(&path));
        assert!(!got);
        assert!(logs.is_empty(), "a missing file must stay silent: {logs}");
    }

    #[test]
    fn a_malformed_config_is_release_and_logs_the_path_and_a_fix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "features: [unclosed");
        let mut got = true;
        let logs = capture_logs(|| got = beta_track_from_config(&path));
        assert!(!got, "an unreadable config must not select beta");
        assert!(logs.contains("config.yaml"), "{logs}");
        assert!(logs.contains("Fix:"), "{logs}");
    }

    #[test]
    fn a_config_without_an_upgrade_track_is_release() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "{}\n");
        let mut got = true;
        let logs = capture_logs(|| got = beta_track_from_config(&path));
        assert!(!got);
        assert!(logs.is_empty(), "a valid config must not log: {logs}");
    }
}
