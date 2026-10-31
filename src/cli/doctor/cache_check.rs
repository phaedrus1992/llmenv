//! The doctor check that the cache directory is writable.

use std::path::Path;

use anyhow::Context as _;

/// Check that `cache_dir` accepts a probe file, and create it owner-only when it is absent.
///
/// A link at `cache_dir` is refused. `exists()` would follow it, so the probe would land in the
/// link's target (#3116).
pub(super) fn check_cache_dir_writable(cache_dir: &Path) -> anyhow::Result<()> {
    let cache_entry = std::fs::symlink_metadata(cache_dir);
    if cache_entry
        .as_ref()
        .is_ok_and(std::fs::Metadata::is_symlink)
    {
        anyhow::bail!(
            "cache directory {} is a link. Fix: make it a real folder, not a link.",
            cache_dir.display()
        );
    }
    if cache_entry.is_ok() {
        let probe = cache_dir.join(".llmenv-doctor-probe");
        crate::paths::write_owner_only(&probe, b"").context("cache directory not writable")?;
        if let Err(e) = std::fs::remove_file(&probe) {
            tracing::error!("could not remove doctor probe {} ({e})", probe.display());
        }
    } else {
        crate::paths::create_dir_owner_only(cache_dir).context("cache directory not writable")?;
    }
    Ok(())
}
