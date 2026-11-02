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
        if p.extension().is_some_and(|e| e == "tmp") {
            gc_remove_dir(&p, &mut report);
            continue;
        }
        // The durable state dir (#175) is a stable sibling of the hashed config
        // folders and must outlive every materialization. It is never an age-based
        // collection candidate — cache invalidation must not wipe tool state.
        if p.file_name()
            == Some(std::ffi::OsStr::new(
                crate::materialize::state::STATE_DIR_NAME,
            ))
        {
            report.kept += 1;
            continue;
        }
        // In Normal mode, a top-level dir matching the current generation version
        // is a container for per-shape leaf dirs. Walk into it and age-check each
        // shape individually instead of treating the whole generation as a unit.
        if hashing == HashingMode::Normal
            && is_current_name(
                p.file_name().unwrap_or_default(),
                hashing,
                current_version.as_deref(),
            )
        {
            gc_normal_shapes(&p, rule, &now, &mut report)?;
            continue;
        }
        let Some(m) = newest_file_mtime_or_record(&p, rule.source, |path, cause| {
            report.record_failure(path, cause)
        }) else {
            continue;
        };
        let expired = is_expired(now, m, rule.older_than, &p);
        if expired {
            gc_remove_dir(&p, &mut report);
        } else {
            report.kept += 1;
        }
    }
    Ok(report)
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
        if ft.is_symlink() || !ft.is_dir() {
            tracing::trace!(
                "gc_normal_shapes: skipping unexpected entry: {}",
                p.display()
            );
            continue;
        }
        // Unconditionally remove .tmp staging directories — they represent
        // orphaned partial writes from a previous crashed materialize call.
        if p.extension().is_some_and(|e| e == "tmp") {
            gc_remove_dir(&p, report);
            continue;
        }
        // The durable state dir (#175) may appear nested under a version dir
        // in some layouts; protect it from age-based collection regardless.
        if p.file_name()
            == Some(std::ffi::OsStr::new(
                crate::materialize::state::STATE_DIR_NAME,
            ))
        {
            report.kept += 1;
            continue;
        }
        let Some(m) = newest_file_mtime_or_record(&p, rule.source, |path, cause| {
            report.record_failure(path, cause)
        }) else {
            continue;
        };
        let expired = is_expired(*now, m, rule.older_than, &p);
        if expired {
            gc_remove_dir(&p, report);
        } else {
            report.kept += 1;
        }
    }
    Ok(())
}
