//! `llmenv clean`: the one cleanup command (#2917). It removes the stale cache folders that
//! `prune` removed, then runs the sweeps that `doctor --gc` ran. `prune` and `doctor --gc` are
//! deprecated forms of it.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::doctor::cache_gc::{self, GcOutput};
use crate::config::Config;
use crate::materialize::cache::PruneMode;
use crate::paths;

/// The line that `llmenv prune` prints before it runs `clean`.
const PRUNE_DEPRECATED: &str = "llmenv prune is deprecated; use llmenv clean";
/// The line that `llmenv doctor --gc` prints before its sweeps.
pub(super) const DOCTOR_GC_DEPRECATED: &str = "llmenv doctor --gc is deprecated; use llmenv clean";

/// The flags of `llmenv clean`, which the deprecated `llmenv prune` shares.
#[derive(Debug, clap::Args)]
pub(super) struct CleanArgs {
    /// Remove ALL cache folders unconditionally (next export re-materializes all environments)
    #[arg(long, conflicts_with = "older_than")]
    all: bool,
    /// In the cache-folder step, remove ONLY current-version cache folders older than this
    /// duration (e.g., "14d", "1w"). The age-based sweeps always use cache_retention_hours
    #[arg(long)]
    older_than: Option<String>,
    /// List every item that a real run tries to remove, and remove nothing
    #[arg(long)]
    dry_run: bool,
    /// Remove the shared plugin cache directory for all adapters (state/<adapter>/state/plugins/)
    #[arg(long)]
    plugin_cache: bool,
}

/// Run `llmenv clean`: the cache-folder cleanup, then every age-based sweep.
///
/// # Errors
/// Fails on a bad flag or an unreadable config before anything is removed. Fails after the
/// sweeps when a cache entry, a plugin cache, or a sweep could not be removed or checked. The
/// sweeps still run after a cache-folder failure, and the cache-folder error comes first.
pub(super) fn run_clean(args: &CleanArgs, use_color: bool) -> anyhow::Result<()> {
    let mode = prune_mode(args)?;
    let config = Config::load(&paths::config_path()?)?;
    let cache_dir = PathBuf::from(paths::expand_tilde(&config.cache.cache_dir)?);
    let pruned = run_prune(args, mode, &config, &cache_dir);
    let (pass, warn) = (
        super::doctor_pass(use_color),
        super::doctor_warning(use_color),
    );
    let out = GcOutput {
        pass: &pass,
        warn: &warn,
        dry_run: args.dry_run,
    };
    let sweep_failures = cache_gc::run(&cache_dir, &config, out);
    pruned?;
    // #1346: a cleanup that could not finish is not a success, even though each failure was
    // printed above. A script that sees exit 0 moves on with the files still there.
    if sweep_failures > 0 {
        anyhow::bail!(
            "{sweep_failures} sweep entry(ies) or sweep(s) could not be checked or removed — see \
             the warnings above"
        );
    }
    Ok(())
}

/// Run the deprecated `llmenv prune`: print the notice, then run `clean` with the same flags.
///
/// # Errors
/// The same as [`run_clean`].
pub(super) fn run_prune_alias(args: &CleanArgs, use_color: bool) -> anyhow::Result<()> {
    eprintln!("{PRUNE_DEPRECATED}");
    run_clean(args, use_color)
}

/// Read the cache-folder mode from the flags. Runs before anything is removed, so a bad
/// `--older-than` value stops the whole command.
fn prune_mode(args: &CleanArgs) -> anyhow::Result<PruneMode> {
    if args.all {
        return Ok(PruneMode::All);
    }
    let Some(raw) = &args.older_than else {
        // Default: folders from other versions, and orphaned *.tmp staging folders.
        return Ok(PruneMode::StaleOnly);
    };
    let duration = humantime::parse_duration(raw).with_context(|| {
        format!("cannot read --older-than {raw:?}: give a duration such as 14d or 1w")
    })?;
    Ok(PruneMode::OlderThan(crate::materialize::expiry::AgeRule {
        older_than: duration,
        source: crate::materialize::cache::AgeSource::MtimeOrCtime,
    }))
}

/// What one step of the cache-folder cleanup removed and could not remove.
#[derive(Debug, Default)]
struct Removals {
    removed: Vec<PathBuf>,
    failed: Vec<PathBuf>,
    kept: usize,
}

/// The `prune` share of `clean`: stale cache folders and, with `--plugin-cache`, the shared
/// plugin caches.
fn run_prune(
    args: &CleanArgs,
    mode: PruneMode,
    config: &Config,
    cache_dir: &Path,
) -> anyhow::Result<()> {
    let verb = if args.dry_run {
        "would remove"
    } else {
        "removed"
    };
    let (folders, walk_error) = prune_folders(config, cache_dir, mode, args.dry_run);
    for p in &folders.removed {
        eprintln!("  {verb}: {}", p.display());
    }
    for p in &folders.failed {
        eprintln!(
            "  could not check or remove (cause logged above): {}",
            p.display()
        );
    }
    eprintln!(
        "prune complete: {verb} {} entry(ies), kept {}",
        folders.removed.len(),
        folders.kept
    );
    // Drop each removed folder's platform credential entry, as the gc share does. Folders deleted
    // without it are how orphaned keychain items accumulate (#1057).
    let mut credentials_failed = 0;
    if !args.dry_run && !folders.removed.is_empty() {
        let drops = cache_gc::forget_credentials_for(&folders.removed);
        if drops.removed > 0 {
            eprintln!(
                "  dropped {} orphaned OAuth credential entry(ies)",
                drops.removed
            );
        }
        if drops.failed > 0 {
            eprintln!(
                "  could not drop {} OAuth credential entry(ies); see the error above",
                drops.failed
            );
        }
        credentials_failed = drops.failed;
    }
    let plugins = if args.plugin_cache {
        remove_plugin_caches(cache_dir, args.dry_run)
    } else {
        Removals::default()
    };
    for p in &plugins.removed {
        eprintln!("  {verb} plugin cache: {}", p.display());
    }
    for p in &plugins.failed {
        eprintln!("  failed to remove plugin cache: {}", p.display());
    }
    if args.plugin_cache {
        eprintln!(
            "plugin cache prune: {verb} {} entry(ies)",
            plugins.removed.len()
        );
    }
    if let Some(e) = walk_error {
        return Err(e);
    }
    // #1346, #1372: a cache entry or a plugin cache that could not be removed fails the command.
    if !folders.failed.is_empty() {
        anyhow::bail!(
            "{} cache entry(ies) could not be removed — see the failures above",
            folders.failed.len()
        );
    }
    if !plugins.failed.is_empty() {
        anyhow::bail!(
            "{} plugin cache entry(ies) could not be removed — see the failures above",
            plugins.failed.len()
        );
    }
    // A credential that could not be dropped is left in the keychain, so the command fails too,
    // as the GC summary already counts it (#3073 review).
    if credentials_failed > 0 {
        anyhow::bail!(
            "{credentials_failed} OAuth credential entry(ies) could not be dropped — see the \
             errors above"
        );
    }
    Ok(())
}

/// Prune the cache folders of every adapter. An adapter whose cache cannot be walked does not
/// stop the others, so the folders already removed still get reported and their credentials
/// dropped. The first walk error comes back with the totals.
fn prune_folders(
    config: &Config,
    cache_dir: &Path,
    mode: PruneMode,
    dry_run: bool,
) -> (Removals, Option<anyhow::Error>) {
    // In normal mode the current generation dir (e.g. "1.2") has no `{VERSION_TAG}-` prefix, so
    // StaleOnly must be told its name or it would sweep the live config dir. Loose mode has no
    // version axis and strict mode is identified by the prefix test, so both pass None.
    let current_version = match config.cache.hashing {
        crate::config::HashingMode::Normal => Some(crate::materialize::cache::version_major()),
        crate::config::HashingMode::Loose | crate::config::HashingMode::Strict => None,
    };
    let mut totals = Removals::default();
    let mut first_error = None;
    // The generation and VERSION_TAG folders live under `<cache_dir>/<adapter>/`.
    for adapter in crate::adapter::registered_adapters() {
        let adapter_dir = cache_dir.join(adapter.name());
        match crate::materialize::cache::prune(
            &adapter_dir,
            mode,
            config.cache.hashing,
            current_version.as_deref(),
            dry_run,
        ) {
            Ok(report) => {
                totals.removed.extend(report.removed);
                totals.failed.extend(report.failed);
                totals.kept += report.kept;
            }
            Err(e) => {
                eprintln!("  could not walk {}: {e:#}", adapter_dir.display());
                first_error.get_or_insert(e.context(format!("pruning {}", adapter_dir.display())));
            }
        }
    }
    (totals, first_error)
}

/// Remove the shared plugin cache folder of each adapter. A folder that is already gone counts
/// as removed.
fn remove_plugin_caches(cache_dir: &Path, dry_run: bool) -> Removals {
    let mut out = Removals::default();
    for adapter in crate::adapter::registered_adapters() {
        let plugins_dir = cache_dir
            .join(adapter.name())
            .join(crate::materialize::state::STATE_DIR_NAME)
            .join("plugins");
        // A folder that is not there is neither removed nor a failure.
        let result = if dry_run {
            std::fs::symlink_metadata(&plugins_dir).map(|_| ())
        } else {
            std::fs::remove_dir_all(&plugins_dir)
        };
        match result {
            Ok(()) => out.removed.push(plugins_dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::error!(
                    path = %plugins_dir.display(),
                    error = %e,
                    "failed to remove plugin cache"
                );
                out.failed.push(plugins_dir);
            }
        }
    }
    out
}
