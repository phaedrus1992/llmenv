//! The sweep of the hashed cache, and the prune of marketplace clones that no config entry names.
//!
//! `cache::gc` ages the whole `marketplaces` folder as one unit, so a clone left behind by a
//! renamed marketplace would stay until the folder itself is old (#3084).

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use super::super::marketplace_names::referenced_marketplaces;
use super::GcOutput;
use super::credentials::report_credential_drops;
use crate::config::{CONTEXT_MODE_MARKETPLACE, Config};
use crate::materialize::cache::{GcReport, gc_remove_dir, gc_remove_link};

/// Sweep the hashed cache under `cache_dir`, then the orphan marketplace clones. Returns the count
/// of failed entries.
pub(super) fn sweep_cache(
    cache_dir: &Path,
    config: &Config,
    retention: Duration,
    out: GcOutput<'_>,
) -> usize {
    match crate::materialize::cache::gc(
        cache_dir,
        crate::materialize::expiry::AgeRule {
            older_than: retention,
            source: crate::materialize::cache::AgeSource::MtimeOrCtime,
        },
        config.cache.hashing,
        out.dry_run,
    ) {
        Ok(report) => {
            out.report("", &report);
            // A dry run deletes nothing, so it drops no credentials and has none to fail (#3073).
            let credential_failures = if out.dry_run {
                0
            } else {
                report_credential_drops(&report.removed, out)
            };
            let orphans = prune_orphan_marketplaces(cache_dir, config, out.dry_run);
            out.report(" (marketplace clones)", &orphans);
            report.failed.len() + credential_failures + orphans.failed.len()
        }
        Err(e) => {
            eprintln!("{} GC failed: {e:#}", out.warn);
            1
        }
    }
}

/// Lowercased clone names that some config entry still needs: a declared marketplace, the
/// marketplace of a plugin reference, and the context-mode marketplace. Matching ignores case,
/// because the default macOS filesystem does, so a clone under another case is kept.
fn referenced_marketplace_names(config: &Config) -> HashSet<String> {
    config
        .marketplace
        .iter()
        .map(|m| m.name.as_str())
        .chain(referenced_marketplaces(config))
        .chain(std::iter::once(CONTEXT_MODE_MARKETPLACE))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The clone that a folder under the marketplace root belongs to. A staging folder
/// (`<name>.<pid>.tmp`) and a backup (`<name>.<pid>.old`) belong to the clone they were made
/// from, so a sync in progress keeps its clone.
fn owning_clone(entry: &str) -> &str {
    for suffix in [".tmp", ".old"] {
        if let Some((name, pid)) = entry
            .strip_suffix(suffix)
            .and_then(|rest| rest.rsplit_once('.'))
            && !pid.is_empty()
            && pid.bytes().all(|b| b.is_ascii_digit())
        {
            return name;
        }
    }
    entry
}

/// Remove each clone under the marketplace folder that no config entry names. A dry run records
/// each one and deletes nothing.
fn prune_orphan_marketplaces(cache_dir: &Path, config: &Config, dry_run: bool) -> GcReport {
    let root = crate::plugins::cache::marketplace_cache_root(cache_dir);
    let mut report = GcReport::new(dry_run);
    let Some(entries) = marketplace_entries(&root, &mut report) else {
        return report;
    };
    let keep = referenced_marketplace_names(config);
    for entry in entries {
        prune_entry(entry, &root, &keep, &mut report);
    }
    report
}

/// The entries of the marketplace folder, or `None` when there is nothing to prune or the folder
/// cannot be read. A symlinked folder is refused, because the walk would read the folder the link
/// points to.
fn marketplace_entries(root: &Path, report: &mut GcReport) -> Option<std::fs::ReadDir> {
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() => {
            report.record_failure(
                root.to_path_buf(),
                "marketplace folder is a symlink, so no clone was pruned. Fix: make it a real folder."
                    .to_owned(),
            );
            return None;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        _ => {}
    }
    match crate::paths::read_dir_optional(root) {
        Ok(entries) => entries,
        Err(e) => {
            report.record_failure(root.to_path_buf(), format!("{e:#}"));
            None
        }
    }
}

/// Remove one entry of the marketplace folder, unless a config entry names it.
fn prune_entry(
    entry: std::io::Result<std::fs::DirEntry>,
    root: &Path,
    keep: &HashSet<String>,
    report: &mut GcReport,
) {
    let entry = match entry {
        Ok(entry) => entry,
        Err(e) => {
            report.record_failure(root.to_path_buf(), format!("{e:#}"));
            return;
        }
    };
    if is_kept(&entry.file_name(), keep) {
        report.kept += 1;
        return;
    }
    let path = entry.path();
    match entry.file_type() {
        Ok(ft) if ft.is_symlink() => gc_remove_link(&path, report),
        Ok(ft) if ft.is_dir() => gc_remove_dir(&path, report),
        Ok(_) => report.kept += 1,
        Err(e) => report.record_failure(path, format!("{e:#}")),
    }
}

/// Whether a folder name is kept. A name that is not UTF-8 matches no config name, so it is not.
fn is_kept(name: &std::ffi::OsStr, keep: &HashSet<String>) -> bool {
    name.to_str()
        .is_some_and(|name| keep.contains(&owning_clone(name).to_ascii_lowercase()))
}

/// Sweep the cache under `cache_dir`, unless its root is a symlink or read-only. Returns the count
/// of failures.
pub(super) fn sweep_cache_root(
    cache_dir: &Path,
    config: &Config,
    retention: Duration,
    out: GcOutput<'_>,
) -> usize {
    // `symlink_metadata` reads the link itself, so a dangling link is seen too. The sweep would
    // judge the link's target and remove entries under it, so a symlink root is refused (#3089).
    match std::fs::symlink_metadata(cache_dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            eprintln!(
                "{} GC of the cache skipped: cache directory {} is a symlink. Fix: set \
                 cache.cache_dir to the real folder.",
                out.warn,
                cache_dir.display()
            );
            1
        }
        Ok(meta) if meta.permissions().readonly() => {
            eprintln!(
                "{} GC of the cache skipped: cache directory is read-only",
                out.warn
            );
            1
        }
        Ok(_) => sweep_cache(cache_dir, config, retention, out),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            eprintln!(
                "{} GC of the cache skipped: cannot stat cache directory: {e:#}",
                out.warn
            );
            1
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test code: a failed setup step should abort the test"
)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::{Marketplace, PluginCollection};

    fn config_with(marketplaces: &[&str], plugins: &[&str]) -> Config {
        Config {
            marketplace: marketplaces
                .iter()
                .map(|name| Marketplace {
                    name: (*name).into(),
                    source: "https://example.com".into(),
                })
                .collect(),
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: plugins.iter().map(|p| (*p).into()).collect(),
            }],
            ..Config::default()
        }
    }

    fn make_clone(cache_dir: &Path, name: &str) -> PathBuf {
        let clone = crate::plugins::cache::marketplace_path(cache_dir, name);
        fs::create_dir_all(&clone).unwrap();
        fs::write(clone.join("marker"), b"x").unwrap();
        clone
    }

    proptest::proptest! {
        #[test]
        fn a_staging_or_backup_folder_belongs_to_the_clone_it_was_made_from(
            name in "[a-z]{1,8}(\\.[a-z]{1,8}){0,2}",
            pid in "[0-9]{1,6}",
        ) {
            let staging = format!("{name}.{pid}.tmp");
            let backup = format!("{name}.{pid}.old");
            proptest::prop_assert_eq!(owning_clone(&staging), name.as_str());
            proptest::prop_assert_eq!(owning_clone(&backup), name.as_str());
        }
    }

    #[test]
    fn a_suffix_that_is_not_a_pid_is_part_of_the_name() {
        assert_eq!(owning_clone("foo.bar.tmp"), "foo.bar.tmp");
        assert_eq!(owning_clone("foo.tmp"), "foo.tmp");
        assert_eq!(owning_clone("foo.12.tmp"), "foo");
    }

    #[test]
    fn an_orphan_clone_is_removed_and_every_referenced_clone_is_kept() {
        let cache = tempfile::tempdir().unwrap();
        let orphan = make_clone(cache.path(), "old-name");
        let declared = make_clone(cache.path(), "used");
        let via_plugin = make_clone(cache.path(), "via");
        let context = make_clone(cache.path(), CONTEXT_MODE_MARKETPLACE);
        let config = config_with(&["used"], &["via:some-plugin"]);

        let report = prune_orphan_marketplaces(cache.path(), &config, false);

        assert_eq!(report.removed, vec![orphan.clone()]);
        assert!(!orphan.exists(), "the orphan clone must be removed");
        assert!(declared.exists() && via_plugin.exists() && context.exists());
    }

    #[test]
    fn a_dry_run_lists_an_orphan_clone_and_deletes_nothing() {
        let cache = tempfile::tempdir().unwrap();
        let orphan = make_clone(cache.path(), "old-name");
        let config = config_with(&[], &[]);

        let report = prune_orphan_marketplaces(cache.path(), &config, true);

        assert_eq!(report.removed, vec![orphan.clone()]);
        assert!(orphan.exists(), "a dry run must not delete");
    }

    #[test]
    fn a_missing_marketplace_folder_is_nothing_to_prune() {
        let cache = tempfile::tempdir().unwrap();
        let config = config_with(&[], &[]);

        let report = prune_orphan_marketplaces(cache.path(), &config, false);

        assert!(report.removed.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn a_staging_folder_of_a_referenced_clone_is_kept() {
        let cache = tempfile::tempdir().unwrap();
        let staging = make_clone(cache.path(), "used.42.tmp");
        let config = config_with(&["used"], &[]);

        let report = prune_orphan_marketplaces(cache.path(), &config, false);

        assert!(
            staging.exists(),
            "a sync in progress must keep its staging folder"
        );
        assert!(report.removed.is_empty());
    }

    #[test]
    fn a_clone_whose_name_differs_only_in_case_is_kept() {
        let cache = tempfile::tempdir().unwrap();
        let clone = make_clone(cache.path(), "foo");
        let config = config_with(&["FOO"], &[]);

        let report = prune_orphan_marketplaces(cache.path(), &config, false);

        assert!(clone.exists(), "a case-only match must keep the clone");
        assert!(report.removed.is_empty());
    }

    #[test]
    fn a_symlinked_marketplace_folder_is_refused_and_its_target_is_untouched() {
        let cache = tempfile::tempdir().unwrap();
        let real = cache.path().join("real");
        let victim = real.join("victim");
        fs::create_dir_all(&victim).unwrap();
        std::os::unix::fs::symlink(
            &real,
            crate::plugins::cache::marketplace_cache_root(cache.path()),
        )
        .unwrap();
        let config = config_with(&[], &[]);

        let report = prune_orphan_marketplaces(cache.path(), &config, false);

        assert_eq!(
            report.failed.len(),
            1,
            "a symlinked folder must be reported"
        );
        assert!(victim.exists(), "the link target must not be pruned");
    }
}
