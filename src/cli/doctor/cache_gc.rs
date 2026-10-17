//! The sweeps behind `llmenv doctor --gc`: the hashed cache, then `state/tmp`, then session-log
//! rotations (#2814, #2816).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::materialize::{cache, tmp_sweep};
use crate::paths;
use crate::session_log;

use super::forget_credentials_for;

/// Run the sweeps for `doctor --gc`. All three use `cache.cache_retention_hours` (default 168).
/// A `null` value turns all three off (#2829).
pub(super) fn run(cache_dir: &Path, config: &Config, pass: &str, warn: &str) {
    eprintln!("Running garbage collection...");
    let Some(retention) = config.cache.gc_retention() else {
        eprintln!(
            "{pass} GC skipped: cache.cache_retention_hours is null, so age-based GC is off. \
             Set it to a number of hours to turn GC on."
        );
        return;
    };
    match std::fs::metadata(cache_dir) {
        Ok(meta) if meta.permissions().readonly() => {
            eprintln!("{warn} GC of the cache skipped: cache directory is read-only");
        }
        Ok(_) => sweep_cache(cache_dir, config, retention, pass, warn),
        Err(e) => eprintln!("{warn} GC of the cache skipped: cannot stat cache directory: {e:#}"),
    }
    match paths::state_dir() {
        Ok(state_dir) => {
            sweep_state(&state_dir, retention, pass, warn);
            sweep_cbm_indexes(&state_dir, config, retention, pass, warn);
        }
        Err(e) => eprintln!(
            "{warn} GC of state/tmp and session-log rotations skipped: cannot resolve the state \
             directory: {e:#}"
        ),
    }
}

/// Sweep the hashed cache under `cache_dir`.
fn sweep_cache(cache_dir: &Path, config: &Config, retention: Duration, pass: &str, warn: &str) {
    match cache::gc(cache_dir, retention, config.cache.hashing) {
        Ok(report) => {
            eprintln!(
                "{pass} GC complete: removed {} entries, kept {}",
                report.removed.len(),
                report.kept
            );
            // #1372: a removal failure no longer aborts the walk,
            // so the entries it skipped have to be reported here
            // or they'd vanish from the summary entirely.
            for p in &report.failed {
                eprintln!("{warn} GC: could not process {}", p.display());
            }
            let forgotten = forget_credentials_for(&report.removed);
            if forgotten > 0 {
                eprintln!(
                    "{pass} GC: dropped {forgotten} orphaned OAuth credential \
                     entries"
                );
            }
        }
        Err(e) => eprintln!("{warn} GC failed: {e:#}"),
    }
}

/// Sweep the folders under `state_dir`: `state/tmp`, then the session-log rotations.
fn sweep_state(state_dir: &Path, retention: Duration, pass: &str, warn: &str) {
    sweep_state_tmp(state_dir, retention, pass, warn);
    prune_session_log(state_dir, retention, pass, warn);
}

/// Sweep `state/tmp`, which `TMPDIR` points at, with the same retention as the cache (#2814).
fn sweep_state_tmp(state_dir: &Path, retention: Duration, pass: &str, warn: &str) {
    match tmp_sweep::sweep_state_tmp(state_dir, retention) {
        Ok(report) => {
            eprintln!(
                "{pass} GC complete for state/tmp: removed {} entries, kept {}",
                report.removed.len(),
                report.kept
            );
            for p in &report.failed {
                eprintln!("{warn} GC: could not process {}", p.display());
            }
        }
        Err(e) => eprintln!("{warn} GC of state/tmp skipped: {e:#}"),
    }
}

/// Remove old index databases from `state/codebase-memory`, unless a configured `index_path`
/// names that folder as the live cache (#2817). A database that a process holds open is kept.
fn sweep_cbm_indexes(
    state_dir: &Path,
    config: &Config,
    retention: Duration,
    pass: &str,
    warn: &str,
) {
    let dir = state_dir.join("codebase-memory");
    match cache_use(config, &dir) {
        CacheUse::Unused => {}
        CacheUse::Live => {
            eprintln!(
                "{pass} GC of {} skipped: a features.codebase_memory index_path uses it",
                dir.display()
            );
            return;
        }
        CacheUse::Unknown(cause) => {
            eprintln!("{warn} GC of {} skipped: {cause}", dir.display());
            return;
        }
    }
    let in_use = match crate::materialize::open_files::open_paths() {
        Ok(in_use) => in_use,
        Err(e) => {
            eprintln!("{warn} GC of old codebase-memory indexes skipped: {e:#}");
            return;
        }
    };
    // lsof reports resolved paths, so the folder is resolved too before the paths are compared.
    let resolved = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    match crate::materialize::cbm_sweep::sweep_legacy_indexes(&resolved, retention, &in_use) {
        Ok(report) => {
            eprintln!(
                "{pass} GC complete for old codebase-memory indexes: removed {} files, kept {}",
                report.removed.len(),
                report.kept
            );
            for p in &report.failed {
                eprintln!("{warn} GC: could not process {}", p.display());
            }
        }
        Err(e) => eprintln!("{warn} GC of old codebase-memory indexes skipped: {e:#}"),
    }
}

/// Whether a configured `index_path` names the codebase-memory folder.
#[derive(Debug, PartialEq, Eq)]
enum CacheUse {
    Unused,
    Live,
    /// An `index_path` cannot be expanded, so the answer is not known.
    Unknown(String),
}

fn cache_use(config: &Config, dir: &Path) -> CacheUse {
    let canonical_dir = std::fs::canonicalize(dir).ok();
    let raws = config
        .features
        .iter()
        .flat_map(|f| &f.codebase_memory)
        .filter_map(|cm| cm.index_path.as_deref());
    for raw in raws {
        let expanded = match paths::expand_tilde(raw) {
            Ok(expanded) => PathBuf::from(expanded),
            Err(e) => return CacheUse::Unknown(format!("cannot expand index_path {raw}: {e:#}")),
        };
        let same = expanded == dir
            || (canonical_dir.is_some() && std::fs::canonicalize(&expanded).ok() == canonical_dir);
        if same {
            return CacheUse::Live;
        }
    }
    CacheUse::Unused
}

/// Remove session-log rotations older than the retention (#2816). The active file is kept.
fn prune_session_log(state_dir: &Path, retention: Duration, pass: &str, warn: &str) {
    let path = session_log::file_sink::file_path_in(state_dir);
    match session_log::rotation::prune_rotations(&path, retention) {
        Ok(count) => eprintln!("{pass} GC: removed {count} rotated session-log file(s)"),
        Err(e) => eprintln!("{warn} GC of session-log rotations failed: {e:#}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    use super::{sweep_cache, sweep_state};
    use crate::config::Config;

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn a_configured_index_path_marks_the_folder_live() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("codebase-memory");
        std::fs::create_dir(&dir).unwrap();
        let with = |index_path: &str| -> Config {
            serde_yaml::from_str(&format!(
                "features:\n  codebase_memory:\n    - index_path: {index_path}\n"
            ))
            .unwrap()
        };
        use super::{CacheUse, cache_use};
        assert_eq!(
            cache_use(&with(&dir.display().to_string()), &dir),
            CacheUse::Live
        );
        assert_eq!(cache_use(&with("/elsewhere/cbm"), &dir), CacheUse::Unused);
        assert_eq!(cache_use(&Config::default(), &dir), CacheUse::Unused);
    }

    #[test]
    fn the_state_sweep_removes_an_old_rotation_and_keeps_the_active_log() {
        let state = tempfile::tempdir().unwrap();
        let active = state.path().join("session-log.jsonl");
        std::fs::write(&active, b"{}").unwrap();
        let rotated = state.path().join("session-log.jsonl.1");
        std::fs::write(&rotated, b"{}").unwrap();
        File::options()
            .write(true)
            .open(&rotated)
            .unwrap()
            .set_modified(SystemTime::now() - 30 * DAY)
            .unwrap();

        sweep_state(state.path(), DAY * 7, "ok", "warn");

        assert!(!rotated.exists(), "the old rotation was kept");
        assert!(active.exists(), "the active log was removed");
    }

    #[test]
    fn the_state_sweep_with_no_tmp_folder_and_no_log_changes_nothing() {
        let state = tempfile::tempdir().unwrap();
        sweep_state(state.path(), DAY * 7, "ok", "warn");
        assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_cache_sweep_keeps_a_fresh_entry() {
        let cache = tempfile::tempdir().unwrap();
        let entry = cache.path().join("fresh");
        std::fs::create_dir(&entry).unwrap();
        sweep_cache(cache.path(), &Config::default(), DAY * 7, "ok", "warn");
        assert!(entry.exists(), "a fresh cache entry was removed");
    }

    #[test]
    fn the_cache_sweep_reports_an_unreadable_cache_and_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        sweep_cache(&not_a_dir, &Config::default(), DAY * 7, "ok", "warn");
        assert!(not_a_dir.exists());
    }
}
