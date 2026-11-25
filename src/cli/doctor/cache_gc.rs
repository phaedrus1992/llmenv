//! The sweeps behind `llmenv clean` and the deprecated `llmenv doctor --gc`: the hashed cache,
//! then `state/tmp`, then session-log rotations, then old codebase-memory indexes (#2814, #2816,
//! #2917).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::materialize::cache::{AgeSource, GcReport};
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

    /// Print each entry of a dry run, so the preview names every item (#2917). With `ages`, each
    /// line also gives the age that the sweep judged for the entry (#4054).
    fn list(&self, removed: &[PathBuf], ages: Option<&BTreeMap<PathBuf, Duration>>) {
        if !self.dry_run {
            return;
        }
        for p in removed {
            match ages.and_then(|ages| ages.get(p)) {
                Some(age) => eprintln!(
                    "  would remove: {} (age {})",
                    p.display(),
                    crate::util::time::format_age(*age)
                ),
                None => eprintln!("  would remove: {}", p.display()),
            }
        }
    }

    /// Print the summary line, the dry-run list, and each failure of one sweep. A sweep that
    /// judges by age gives its `ages` in the report, and the dry-run line shows them.
    fn report(&self, what: &str, report: &GcReport) {
        let (pass, warn) = (self.pass, self.warn);
        eprintln!(
            "{pass} GC complete{what}: {} {} entries, kept {}",
            self.verb(),
            report.removed.len(),
            report.kept
        );
        self.list(&report.removed, Some(&report.ages));
        // #1372: a removal failure does not abort the walk, so the skipped entries are named here.
        for (p, cause) in &report.failed {
            eprintln!("{warn} GC: could not process {}: {cause}", p.display());
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

/// Run every sweep, and return how many entries or sweeps failed. The `state/tmp` sweep uses
/// `tmp_older_than`, else `cache.tmp_retention_hours` (default 168). The other sweeps use
/// `cache.cache_retention_hours` (default 168). A `null` value turns off the sweeps that it
/// governs (#2829, #4046).
///
/// A failure is printed as a warning here. `llmenv clean` turns a nonzero count into an error, so
/// a script does not read a skipped sweep as a finished one.
pub(in crate::cli) fn run(
    cache_dir: &Path,
    config: &Config,
    out: GcOutput<'_>,
    tmp_older_than: Option<Duration>,
) -> usize {
    eprintln!("Running garbage collection...");
    let retention = Retention::resolve(config, tmp_older_than);
    if retention.general.is_none() {
        eprintln!(
            "{} GC of the cache, session-log rotations, and codebase-memory indexes skipped: \
             cache.cache_retention_hours is null, so age-based GC is off. Set it to a number of \
             hours to turn GC on.",
            out.pass
        );
    }
    if retention.tmp.is_none() {
        let null_setting = if config.cache.cache_retention_hours.is_none() {
            "cache.cache_retention_hours"
        } else {
            "cache.tmp_retention_hours"
        };
        eprintln!(
            "{} GC of state/tmp skipped: {null_setting} is null, so the age-based sweep of \
             state/tmp is off. Set it to a number of hours, or give --tmp-older-than, to turn it \
             on.",
            out.pass
        );
    }
    let mut failures = match retention.general {
        Some(general) => sweep_cache_root(cache_dir, config, general, out),
        None => 0,
    };
    if retention.general.is_none() && retention.tmp.is_none() {
        return failures;
    }
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

/// The age of each group of sweeps. `None` turns the group off. `state/tmp` has its own age,
/// because a scratch file needs a shorter keep time than a cache folder (#4046).
#[derive(Debug, Clone, Copy)]
struct Retention {
    /// The cache folders, the session-log rotations, and the codebase-memory indexes.
    general: Option<Duration>,
    /// `state/tmp`.
    tmp: Option<Duration>,
}

impl Retention {
    /// The ages from the config. A `tmp_older_than` from the command line replaces the age of
    /// `state/tmp` only.
    fn resolve(config: &Config, tmp_older_than: Option<Duration>) -> Self {
        Self {
            general: config.cache.gc_retention(),
            tmp: tmp_older_than.or_else(|| config.cache.tmp_retention()),
        }
    }
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
    retention: Retention,
    now: SystemTime,
    out: GcOutput<'_>,
) -> usize {
    let cutoff = |retention| Cutoff {
        retention,
        now,
        source: STATE_AGE_SOURCE,
    };
    let (tmp, general) = (retention.tmp.map(cutoff), retention.general.map(cutoff));
    let rotations = |out| {
        retention.general.map_or(0, |general| {
            prune_session_log(state_dir, config, general, now, out)
        })
    };
    // With nothing old enough to remove, the tmp and index sweeps are skipped. A sweep that ran
    // now could see an entry that crossed the cutoff after this check, and remove it without
    // the in-use list.
    let cbm = cbm_use(config, &state_dir.join("codebase-memory"));
    let cbm_live = matches!(cbm, CacheUse::Live);
    // An unreadable config cannot say whether the codebase-memory folder is live. The sweep then
    // runs, so it reports that the folder could not be checked, and the gate does not hide it.
    let cbm_unknown = general.is_some() && matches!(cbm, CacheUse::Unknown(_));
    if !cbm_unknown && !state_may_have_stale_entries(state_dir, tmp, general, cbm_live) {
        return rotations(out);
    }
    let in_use = crate::materialize::open_files::open_paths().map_err(|e| format!("{e:#}"));
    let in_use = in_use.as_ref().map(Vec::as_slice).map_err(String::as_str);
    tmp.map_or(0, |cutoff| sweep_state_tmp(state_dir, cutoff, in_use, out))
        + rotations(out)
        + general.map_or(0, |cutoff| {
            sweep_cbm_indexes(state_dir, config, cutoff, in_use, out)
        })
}

/// The retention of the state sweeps and the one clock that judges every age, so the gate and
/// each sweep agree on which entry is old (#3374).
#[derive(Clone, Copy)]
struct Cutoff {
    retention: Duration,
    now: SystemTime,
    source: AgeSource,
}

/// What an entry age is read from in the state sweeps: the later of mtime and change time, so a
/// restored entry is kept (#3275). The gate and each sweep read it from their [`Cutoff`], so they
/// cannot disagree (#4054).
const STATE_AGE_SOURCE: AgeSource = AgeSource::MtimeOrCtime;

impl Cutoff {
    /// The age rule of the state sweeps: the retention, read from the source of the cutoff.
    fn rule(self) -> crate::materialize::expiry::AgeRule {
        crate::materialize::expiry::AgeRule {
            older_than: self.retention,
            source: self.source,
        }
    }
}

/// Sweep `state/tmp`, which `TMPDIR` points at (#2814). Its age is `cache.tmp_retention_hours`
/// (#4046).
fn sweep_state_tmp(
    state_dir: &Path,
    cutoff: Cutoff,
    in_use: Result<&[PathBuf], &str>,
    out: GcOutput<'_>,
) -> usize {
    let swept = in_use.map_err(str::to_string).and_then(|in_use| {
        tmp_sweep::sweep(
            &state_dir.join("tmp"),
            cutoff.rule(),
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
            cutoff.rule(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tmp_retention_tests;
