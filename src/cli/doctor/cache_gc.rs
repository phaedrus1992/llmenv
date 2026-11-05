//! The sweeps behind `llmenv clean` and the deprecated `llmenv doctor --gc`: the hashed cache,
//! then `state/tmp`, then session-log rotations, then old codebase-memory indexes (#2814, #2816,
//! #2917).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::materialize::cache::GcReport;
use crate::materialize::tmp_sweep;
use crate::paths;

/// How the sweeps print their result, and whether they remove anything.
#[derive(Debug, Clone, Copy)]
pub(in crate::cli) struct GcOutput<'a> {
    pub(in crate::cli) pass: &'a str,
    pub(in crate::cli) warn: &'a str,
    /// List what a real run removes, and remove nothing (#2917).
    pub(in crate::cli) dry_run: bool,
}

impl GcOutput<'_> {
    fn verb(&self) -> &'static str {
        if self.dry_run {
            "would remove"
        } else {
            "removed"
        }
    }

    /// Print each entry of a dry run, so the preview names every item (#2917).
    fn list(&self, removed: &[PathBuf]) {
        if self.dry_run {
            for p in removed {
                eprintln!("  would remove: {}", p.display());
            }
        }
    }

    /// Print the summary line, the dry-run list, and each failure of one sweep.
    fn report(&self, what: &str, report: &GcReport) {
        let (pass, warn) = (self.pass, self.warn);
        eprintln!(
            "{pass} GC complete{what}: {} {} entries, kept {}",
            self.verb(),
            report.removed.len(),
            report.kept
        );
        self.list(&report.removed);
        // #1372: a removal failure does not abort the walk, so the skipped entries are named here.
        for p in &report.failed {
            match report.causes.get(p) {
                Some(cause) => eprintln!("{warn} GC: could not process {}: {cause}", p.display()),
                None => eprintln!("{warn} GC: could not process {}", p.display()),
            }
        }
    }
}

mod cache_sweep;
mod credentials;
mod index_use;
mod session_rotation;
mod state_gate;
use cache_sweep::sweep_cache_root;
pub(in crate::cli) use credentials::forget_credentials_for;
#[cfg(test)]
use index_use::cache_use;
use index_use::{CacheUse, cbm_use};
use session_rotation::prune_session_log;
use state_gate::state_may_have_stale_entries;

/// Run every sweep, and return how many entries or sweeps failed. All of them use
/// `cache.cache_retention_hours` (default 168). A `null` value turns all of them off (#2829).
///
/// A failure is printed as a warning here. `llmenv clean` turns a nonzero count into an error, so
/// a script does not read a skipped sweep as a finished one.
pub(in crate::cli) fn run(cache_dir: &Path, config: &Config, out: GcOutput<'_>) -> usize {
    eprintln!("Running garbage collection...");
    let Some(retention) = config.cache.gc_retention() else {
        eprintln!(
            "{} GC skipped: cache.cache_retention_hours is null, so age-based GC is off. \
             Set it to a number of hours to turn GC on.",
            out.pass
        );
        return 0;
    };
    let mut failures = sweep_cache_root(cache_dir, config, retention, out);
    match paths::state_dir() {
        Ok(state_dir) => {
            failures += sweep_state(&state_dir, config, retention, SystemTime::now(), out);
        }
        Err(e) => {
            eprintln!(
                "{} GC of state/tmp, session-log rotations, and codebase-memory indexes skipped: \
                 cannot resolve the state directory: {e:#}",
                out.warn
            );
            failures += 1;
        }
    }
    failures
}

/// Sweep the folders under `state_dir`: `state/tmp`, the session-log rotations, and the old
/// codebase-memory indexes. Returns the count of failures.
///
/// The in-use list comes from one lsof call, which the two sweeps share. lsof can take up to a
/// minute on a hung mount, so the call runs only when a sweep may remove something (#3039).
/// `now` judges the session-log rotations. Production passes the clock, and a test passes a later
/// time, so a rotation whose mtime and change time are both new counts as old (#3335).
fn sweep_state(
    state_dir: &Path,
    config: &Config,
    retention: Duration,
    now: SystemTime,
    out: GcOutput<'_>,
) -> usize {
    // With nothing old enough to remove, the tmp and index sweeps are skipped. A sweep that ran
    // now could see an entry that crossed the cutoff after this check, and remove it without
    // the in-use list.
    let cbm = cbm_use(config, &state_dir.join("codebase-memory"));
    let cbm_live = matches!(cbm, CacheUse::Live);
    // An unreadable config cannot say whether the codebase-memory folder is live. The sweep then
    // runs, so it reports that the folder could not be checked, and the gate does not hide it.
    let cbm_unknown = matches!(cbm, CacheUse::Unknown(_));
    if !cbm_unknown
        && !state_may_have_stale_entries(
            state_dir,
            retention,
            cbm_live,
            crate::materialize::cache::AgeSource::MtimeOrCtime,
            now,
        )
    {
        return prune_session_log(state_dir, config, retention, now, out);
    }
    let in_use = crate::materialize::open_files::open_paths().map_err(|e| format!("{e:#}"));
    let in_use = in_use.as_ref().map(Vec::as_slice).map_err(String::as_str);
    let cutoff = Cutoff { retention, now };
    sweep_state_tmp(state_dir, cutoff, in_use, out)
        + prune_session_log(state_dir, config, retention, now, out)
        + sweep_cbm_indexes(state_dir, config, cutoff, in_use, out)
}

/// The retention of the state sweeps and the one clock that judges every age, so the gate and
/// each sweep agree on which entry is old (#3374).
#[derive(Clone, Copy)]
struct Cutoff {
    retention: Duration,
    now: SystemTime,
}

/// Sweep `state/tmp`, which `TMPDIR` points at, with the same retention as the cache (#2814).
fn sweep_state_tmp(
    state_dir: &Path,
    cutoff: Cutoff,
    in_use: Result<&[PathBuf], &str>,
    out: GcOutput<'_>,
) -> usize {
    let swept = in_use.map_err(str::to_string).and_then(|in_use| {
        tmp_sweep::sweep(
            &state_dir.join("tmp"),
            crate::materialize::expiry::AgeRule {
                older_than: cutoff.retention,
                source: crate::materialize::cache::AgeSource::MtimeOrCtime,
            },
            in_use,
            out.dry_run,
            cutoff.now,
        )
        .map_err(|e| format!("{e:#}"))
    });
    match swept {
        Ok(report) => {
            out.report(" for state/tmp", &report);
            report.failed.len()
        }
        Err(e) => {
            eprintln!("{} GC of state/tmp skipped: {e}", out.warn);
            1
        }
    }
}

/// Remove old index databases from `state/codebase-memory`, unless a configured `index_path`
/// names that folder as the live cache (#2817). A database that a process holds open is kept.
fn sweep_cbm_indexes(
    state_dir: &Path,
    config: &Config,
    cutoff: Cutoff,
    in_use: Result<&[PathBuf], &str>,
    out: GcOutput<'_>,
) -> usize {
    let (pass, warn) = (out.pass, out.warn);
    let dir = state_dir.join("codebase-memory");
    // lsof reports resolved paths, so the folder is resolved too before the paths are compared.
    let resolved = match std::fs::canonicalize(&dir) {
        Ok(resolved) => resolved,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            eprintln!(
                "{warn} GC of {} skipped: cannot resolve the folder: {e}",
                dir.display()
            );
            return 1;
        }
    };
    match cbm_use(config, &dir) {
        CacheUse::Unused => {}
        CacheUse::Live => {
            eprintln!(
                "{pass} GC of {} skipped: a features.codebase_memory index_path uses it",
                dir.display()
            );
            return 0;
        }
        CacheUse::Unknown(cause) => {
            eprintln!("{warn} GC of {} skipped: {cause}", dir.display());
            return 1;
        }
    }
    let swept = in_use.map_err(str::to_string).and_then(|in_use| {
        crate::materialize::cbm_sweep::sweep_legacy_indexes(
            &resolved,
            crate::materialize::expiry::AgeRule {
                older_than: cutoff.retention,
                source: crate::materialize::cache::AgeSource::MtimeOrCtime,
            },
            in_use,
            out.dry_run,
            cutoff.now,
        )
        .map_err(|e| format!("{e:#}"))
    });
    match swept {
        Ok(report) => {
            out.report(" for old codebase-memory indexes", &report);
            report.failed.len()
        }
        Err(e) => {
            eprintln!("{warn} GC of old codebase-memory indexes skipped: {e}");
            1
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
