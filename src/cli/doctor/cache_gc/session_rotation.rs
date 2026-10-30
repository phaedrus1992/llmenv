//! Removes old session-log rotations from the state folder (#2816).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::doctor::cache_gc::GcOutput;
use crate::config::Config;
use crate::session_log;

/// Remove session-log rotations older than the retention (#2816), for the default path and for a
/// configured `session_log.file.path` (#3138). The active files are kept. Returns the count of
/// failed passes.
pub(super) fn prune_session_log(
    state_dir: &Path,
    config: &Config,
    retention: Duration,
    out: GcOutput<'_>,
) -> usize {
    let (paths, failures) = log_paths(state_dir, config.session_log_resolved().file_path());
    if let Some(e) = &failures {
        eprintln!(
            "{} GC of configured session-log rotations skipped: {e:#}. Fix: set HOME, or set \
             session_log.file.path to an absolute path, then run `llmenv clean` again.",
            out.warn
        );
    }
    usize::from(failures.is_some())
        + paths
            .iter()
            .map(|path| prune_one(path, retention, out))
            .sum::<usize>()
}

/// The session-log paths to prune: the default path, and the configured path when it differs.
/// An unresolvable configured path comes back as the error, and the default is still pruned.
fn log_paths(state_dir: &Path, configured: Option<&str>) -> (Vec<PathBuf>, Option<anyhow::Error>) {
    let mut paths = vec![session_log::file_sink::file_path_in(state_dir)];
    let Some(raw) = configured else {
        return (paths, None);
    };
    match session_log::resolve_file_path(Some(raw)) {
        Ok(path) if !paths.contains(&path) => {
            paths.push(path);
            (paths, None)
        }
        Ok(_) => (paths, None),
        Err(e) => (paths, Some(e)),
    }
}

fn prune_one(path: &Path, retention: Duration, out: GcOutput<'_>) -> usize {
    match session_log::rotation::prune_rotations(path, retention, out.dry_run) {
        Ok(removed) => {
            eprintln!(
                "{} GC: {} {} rotated session-log file(s) of {}",
                out.pass,
                out.verb(),
                removed.len(),
                path.display()
            );
            out.list(&removed);
            0
        }
        Err(e) => {
            eprintln!(
                "{} GC of session-log rotations of {} failed: {e:#}",
                out.warn,
                path.display()
            );
            1
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_configured_path_prunes_the_default_only() {
        let (paths, err) = log_paths(Path::new("/state"), None);
        assert!(err.is_none());
        assert_eq!(paths, vec![PathBuf::from("/state/session-log.jsonl")]);
    }

    #[test]
    fn a_configured_path_is_pruned_with_the_default() {
        let (paths, err) = log_paths(Path::new("/state"), Some("/logs/s.jsonl"));
        assert!(err.is_none());
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/state/session-log.jsonl"),
                PathBuf::from("/logs/s.jsonl")
            ]
        );
    }

    #[test]
    fn a_configured_default_path_is_pruned_once() {
        let (paths, _) = log_paths(Path::new("/state"), Some("/state/session-log.jsonl"));
        assert_eq!(paths.len(), 1);
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// The default path comes first, no path repeats, and an error leaves the default.
            #[test]
            fn the_default_comes_first_and_no_path_repeats(
                configured in proptest::option::of(prop_oneof![
                    Just("/state/session-log.jsonl".to_owned()),
                    "/[a-z]{1,8}/[a-z]{1,8}\\.jsonl",
                    "[a-z]{1,8}\\.jsonl",
                ]),
            ) {
                let default = PathBuf::from("/state/session-log.jsonl");
                let (paths, err) = log_paths(Path::new("/state"), configured.as_deref());
                prop_assert_eq!(&paths[0], &default);
                prop_assert!(paths.len() <= 2);
                prop_assert!(paths.len() < 2 || paths[0] != paths[1]);
                if err.is_some() {
                    prop_assert_eq!(paths, vec![default]);
                }
            }
        }
    }

    #[test]
    fn a_relative_configured_path_keeps_the_default_and_reports_the_error() {
        let (paths, err) = log_paths(Path::new("/state"), Some("logs/s.jsonl"));
        assert_eq!(paths, vec![PathBuf::from("/state/session-log.jsonl")]);
        assert!(format!("{:#}", err.unwrap()).contains("session_log.file.path"));
    }
}
