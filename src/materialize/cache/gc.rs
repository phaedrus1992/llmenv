//! The age sweep of a cache root, for `llmenv clean` and `doctor --gc` (#2917).

use std::path::Path;
use std::time::SystemTime;

use crate::config::HashingMode;
use crate::materialize::cache::{
    GcReport, entry_and_type, gc_remove_dir, gc_remove_link, is_current_name, is_expired,
    newest_file_mtime_or_record, version_major,
};

/// Remove cache subdirectories that the rule expires, judged by the rule's age source (#3195).
/// `*.tmp` staging directories are removed regardless of age — they represent
/// orphaned partial writes from a previous crashed `materialize` call.
///
/// `hashing` controls how folders are identified and whether per-shape
/// collection applies (#738):
/// - [`HashingMode::Loose`] — every shape folder is a direct child of
///   `cache_root`; each is age-checked individually.
/// - [`HashingMode::Normal`] — direct children are `<version_major>` generation
///   dirs. When a child matches the current generation (computed internally
///   from [`version_major()`]), [`gc_normal_shapes`] walks into it and age-checks
///   each per-shape leaf dir individually, so a single stale shape doesn't
///   block cleanup of the others. Non-current generation dirs are age-checked
///   as a unit (they'll be collected entirely next upgrade).
/// - [`HashingMode::Strict`] — each `{VERSION_TAG}-{content_hash}` folder is a
///   direct child; each is age-checked individually.
///
/// With `dry_run`, the report lists the same entries and nothing is removed (#2917).
pub fn gc(
    cache_root: &Path,
    rule: crate::materialize::expiry::AgeRule,
    hashing: HashingMode,
    dry_run: bool,
) -> anyhow::Result<GcReport> {
    let mut report = GcReport::new(dry_run);
    // #918: a missing cache dir → nothing to gc; a permission error propagates.
    let Some(entries) = crate::paths::read_dir_optional(cache_root)? else {
        return Ok(report);
    };
    let now = SystemTime::now();
    let current_version = match hashing {
        HashingMode::Normal => Some(version_major()),
        _ => None,
    };
    for entry in entries {
        let Some((p, ft)) = entry_and_type(cache_root, entry, &mut report.failed) else {
            continue;
        };
        // Use `file_type()` (lstat-equivalent) — a symlink at the top level
        // is never treated as a cache directory we own. Removing one removes
        // only the link, never the target.
        if ft.is_symlink() {
            gc_remove_link(&p, &mut report);
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        // A path from `read_dir` always ends in a name; a missing one is a failure, not a match.
        let Some(name) = p.file_name() else {
            report.record_failure(p.clone(), "the entry has no file name".into());
            continue;
        };
        // In Normal mode, a top-level dir matching the current generation version
        // is a container for per-shape leaf dirs. Walk into it and age-check each
        // shape individually instead of treating the whole generation as a unit.
        let is_generation = !is_staging_or_state(&p)
            && hashing == HashingMode::Normal
            && is_current_name(name, hashing, current_version.as_deref());
        if is_generation {
            if let Err(e) = gc_normal_shapes(&p, rule, &now, &mut report) {
                tracing::error!(
                    "cache gc cannot sweep the generation {}, so it is kept: {e:#}. Fix: check the permissions of that folder.",
                    p.display()
                );
                report.record_failure(p.clone(), format!("{e:#}"));
            }
            continue;
        }
        collect_if_expired(&p, rule, now, &mut report);
    }
    Ok(report)
}

/// Whether `p` is an orphaned `.tmp` staging folder or the durable state folder, which
/// [`collect_if_expired`] handles before any age check.
fn is_staging_or_state(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "tmp")
        || p.file_name()
            == Some(std::ffi::OsStr::new(
                crate::materialize::state::STATE_DIR_NAME,
            ))
}

/// Collect one cache folder: an orphaned `.tmp` staging folder always, the durable state folder
/// never, and any other folder when it is older than `rule`. One helper serves the top level and
/// the shapes of a generation, so the two cannot drift (#3277).
fn collect_if_expired(
    p: &Path,
    rule: crate::materialize::expiry::AgeRule,
    now: SystemTime,
    report: &mut GcReport,
) {
    // A `.tmp` folder is a partial write from a materialize that crashed.
    if p.extension().is_some_and(|e| e == "tmp") {
        gc_remove_dir(p, report);
        return;
    }
    // The durable state folder (#175) must outlive every materialization, at the top level and
    // nested under a generation alike, so it is never an age-based candidate.
    if p.file_name()
        == Some(std::ffi::OsStr::new(
            crate::materialize::state::STATE_DIR_NAME,
        ))
    {
        report.kept += 1;
        return;
    }
    let Some(m) = newest_file_mtime_or_record(p, rule.source, |path, cause| {
        report.record_failure(path, cause)
    }) else {
        return;
    };
    if is_expired(now, m, rule.older_than, p) {
        gc_remove_dir(p, report);
    } else {
        report.kept += 1;
    }
}

/// Age-check each per-shape leaf directory under a `<version_major>` generation
/// dir individually, rather than collecting the whole generation at once.
///
/// This is the per-shape collection for [`HashingMode::Normal`] (#738): a
/// stale shape leaf (all its files older than `older_than`) is cleaned up
/// while a live shape in the same generation is preserved. The durable state
/// dir is also protected from collection.
fn gc_normal_shapes(
    version_dir: &Path,
    rule: crate::materialize::expiry::AgeRule,
    now: &SystemTime,
    report: &mut GcReport,
) -> anyhow::Result<()> {
    for entry in match std::fs::read_dir(version_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                "gc_normal_shapes: version dir vanished: {}",
                version_dir.display()
            );
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(format!("reading version dir {}", version_dir.display())));
        }
    } {
        let Some((p, ft)) = entry_and_type(version_dir, entry, &mut report.failed) else {
            continue;
        };
        // A file or a link inside a generation is not a shape. It is kept and counted, so the
        // report adds up to every entry (#3277).
        if ft.is_symlink() || !ft.is_dir() {
            tracing::debug!("cache gc keeps a non-shape entry: {}", p.display());
            report.kept += 1;
            continue;
        }
        collect_if_expired(&p, rule, *now, report);
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use crate::materialize::expiry::AgeRule;
    use std::time::Duration;

    // #3277: every entry of a generation is in the report, a file or a link as kept.
    #[test]
    fn a_file_and_a_link_in_a_generation_are_kept_and_counted() {
        let root = tempfile::tempdir().unwrap();
        let generation = root.path().join(version_major());
        std::fs::create_dir_all(generation.join("shape-a")).unwrap();
        std::fs::write(generation.join("shape-a/f"), b"x").unwrap();
        std::fs::write(generation.join("stray.txt"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path(), generation.join("link")).unwrap();
        let report = gc(
            root.path(),
            AgeRule::new(Duration::from_secs(3600)),
            HashingMode::Normal,
            true,
        )
        .unwrap();
        let entries = if cfg!(unix) { 3 } else { 2 };
        assert_eq!(report.kept, entries, "{report:?}");
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.failed.is_empty(), "{report:?}");
    }
}
