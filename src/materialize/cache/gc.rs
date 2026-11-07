//! The age sweep of a cache root, for `llmenv clean` and `doctor --gc` (#2917).

use std::path::Path;
use std::time::SystemTime;

use crate::config::HashingMode;
use crate::materialize::cache::{
    GcReport, entry_and_type, gc_remove_dir, gc_remove_link, is_current_name, is_expired,
    newest_file_mtime_or_record, version_major,
};

/// Remove cache subdirectories that the rule expires, judged by the rule's age source (#3195).
/// `*.tmp` staging directories are removed when orphaned (see [`staging_is_orphan`]) — they represent
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
        let Some((p, ft)) = entry_and_type(cache_root, entry, |p, cause| {
            report.record_failure(p, cause)
        }) else {
            continue;
        };
        // Use `file_type()` (lstat-equivalent) — a symlink at the top level
        // is never treated as a cache directory we own. Removing one removes
        // only the link, never the target.
        if ft.is_symlink() {
            gc_remove_link(&p, &mut report);
            continue;
        }
        // A file at the top level is not a cache folder. It is kept and counted, so the report
        // covers every entry (#3277).
        if !ft.is_dir() {
            tracing::debug!(
                "cache gc keeps a file that is not a cache folder: {}",
                p.display()
            );
            report.kept += 1;
            continue;
        }
        // A path from `read_dir` always ends in a name; a missing one is a failure, not a match.
        let Some(name) = p.file_name() else {
            tracing::error!(
                "cache gc: the entry {} has no file name, so it is kept",
                p.display()
            );
            report.record_failure(p.clone(), "the entry has no file name".into());
            continue;
        };
        // In Normal mode, a top-level dir matching the current generation version
        // is a container for per-shape leaf dirs. Walk into it and age-check each
        // shape individually instead of treating the whole generation as a unit.
        // A generation name is a version number, so it is never a `.tmp` or the state folder.
        let is_generation = hashing == HashingMode::Normal
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

/// A staging folder younger than this may still belong to a running materialize, even when its
/// pid cannot be read. A large tree can take many minutes to write.
const STAGING_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Whether a `.tmp` staging folder is an orphan that gc may remove. It is one when its writer is
/// gone, or when it is older than [`STAGING_GRACE`]. A name with no readable pid is judged by age.
fn staging_is_orphan(p: &Path, now: SystemTime) -> bool {
    let owner_gone = staging_pid(p).and_then(crate::mcp::proxy::is_alive) == Some(false);
    let old = std::fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|age| age > STAGING_GRACE);
    owner_gone || old
}

/// The pid in a staging name `{folder}.{pid}.{nanos}.tmp`, or `None` when the name has no pid.
fn staging_pid(p: &Path) -> Option<u32> {
    let name = p.file_name()?.to_str()?.strip_suffix(".tmp")?;
    let mut parts = name.rsplitn(3, '.');
    let _nanos = parts.next()?;
    parts.next()?.parse().ok()
}

/// Collect one cache folder: an orphaned `.tmp` staging folder, the durable state folder
/// never, and any other folder when it is older than `rule`. One helper serves the top level and
/// the shapes of a generation, so the two cannot drift (#3277).
fn collect_if_expired(
    p: &Path,
    rule: crate::materialize::expiry::AgeRule,
    now: SystemTime,
    report: &mut GcReport,
) {
    // A `.tmp` folder is a partial write from a materialize that crashed, or one that is still
    // running. Only the first is removed.
    if p.extension().is_some_and(|e| e == "tmp") {
        if staging_is_orphan(p, now) {
            gc_remove_dir(p, report);
        } else {
            report.kept += 1;
        }
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
        let Some((p, ft)) = entry_and_type(version_dir, entry, |p, cause| {
            report.record_failure(p, cause)
        }) else {
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

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        /// Each top-level entry lands in exactly one of removed, kept, or failed: a `.tmp` folder
        /// is removed, the state folder and a file are kept, and a folder is removed exactly when
        /// it is older than the rule (#3277).
        #[test]
        fn every_entry_is_removed_kept_or_failed_once(
            kinds in proptest::collection::vec(0u8..5, 0..8),
        ) {
            let root = tempfile::tempdir().unwrap();
            let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
            let mut want_removed = 0usize;
            for (i, kind) in kinds.iter().enumerate() {
                match kind {
                    0 => {
                        // A writer pid that is gone makes the staging folder an orphan.
                        std::fs::create_dir(root.path().join(format!("s{i}.2147483647.1.tmp")))
                            .unwrap();
                        want_removed += 1;
                    }
                    1 => std::fs::write(root.path().join(format!("f{i}")), b"x").unwrap(),
                    2 | 3 => {
                        let dir = root.path().join(format!("d{i}"));
                        std::fs::create_dir(&dir).unwrap();
                        std::fs::write(dir.join("f"), b"x").unwrap();
                        if *kind == 2 {
                            std::fs::File::open(dir.join("f")).unwrap().set_modified(old).unwrap();
                            want_removed += 1;
                        }
                    }
                    _ => {
                        let state = root.path().join(crate::materialize::state::STATE_DIR_NAME);
                        std::fs::create_dir_all(&state).unwrap();
                    }
                }
            }
            let entries = std::fs::read_dir(root.path()).unwrap().count();
            let rule = AgeRule {
                older_than: Duration::from_secs(86_400),
                source: crate::materialize::cache::AgeSource::MtimeOnly,
            };
            let report = gc(root.path(), rule, HashingMode::Strict, true).unwrap();
            proptest::prop_assert_eq!(report.removed.len() + report.kept + report.failed.len(), entries);
            proptest::prop_assert_eq!(report.removed.len(), want_removed);
        }
    }

    /// The pid of a staging name is the middle field, so a dot in the folder name does not move it.
    #[test]
    fn staging_pid_reads_the_field_before_the_nanos() {
        assert_eq!(staging_pid(Path::new("my.folder.4242.17.tmp")), Some(4242));
        assert_eq!(staging_pid(Path::new("folder.tmp")), None);
        assert_eq!(staging_pid(Path::new("folder.x.y.tmp")), None);
    }

    /// A staging folder whose writer is alive is kept, even though its name says it is a partial
    /// write. Removing it would break a materialize that is still running.
    #[test]
    fn a_staging_folder_of_a_live_writer_is_kept() {
        let root = tempfile::TempDir::new().unwrap();
        let staging = root
            .path()
            .join(format!("pending.{}.1.tmp", std::process::id()));
        std::fs::create_dir(&staging).unwrap();
        assert!(!staging_is_orphan(&staging, SystemTime::now()));
    }

    /// A staging folder whose writer is gone is an orphan, and gc removes it at once.
    #[test]
    fn a_staging_folder_of_a_dead_writer_is_an_orphan() {
        let root = tempfile::TempDir::new().unwrap();
        let staging = root.path().join("pending.2147483647.1.tmp");
        std::fs::create_dir(&staging).unwrap();
        assert!(staging_is_orphan(&staging, SystemTime::now()));
    }
}
