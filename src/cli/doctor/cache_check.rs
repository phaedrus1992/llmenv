//! The doctor check that the cache directory exists and accepts a write.

use std::path::Path;

use anyhow::Context as _;

/// Create the cache directory if it is absent, otherwise probe that llmenv can
/// write into it.
///
/// Hardening only applies when this creates the directory; an existing one
/// keeps whatever permissions its owner set — forcing 0700 here would hard-fail
/// on a cache dir shared with a different uid, the same regression #1196 walked
/// back for `codebase_memory.index_path` (#1198).
///
/// # Errors
///
/// A stat error other than "not found" — permission denied on a parent, an
/// unreachable mount — is reported as the stat failure it is (#1445, the same
/// class as #1436). `exists()` folded those into "does not exist", which sent
/// a present-but-unreachable cache dir into the create branch and surfaced it
/// under the generic "not writable" message; that reads as a permissions
/// problem on the directory itself rather than on the path leading to it.
/// Also errors when the path exists but is not a directory, and when the write
/// probe or the directory creation fails.
pub(super) fn ensure_cache_dir_writable(cache_dir: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(cache_dir) {
        // A link is refused: the probe would land in the link's target (#3116).
        Ok(meta) if meta.is_symlink() => anyhow::bail!(
            "cache directory {} is a link. Fix: make it a real folder, not a link.",
            crate::util::display_safe(&cache_dir.display().to_string())
        ),
        Ok(meta) if meta.is_dir() => {
            let probe = cache_dir.join(".llmenv-doctor-probe");
            crate::paths::write_owner_only(&probe, b"").context("cache directory not writable")?;
            if let Err(e) = std::fs::remove_file(&probe) {
                tracing::error!("could not remove doctor probe {} ({e})", probe.display());
            }
        }
        Ok(_) => anyhow::bail!(
            "cache directory {} is not a directory",
            crate::util::display_safe(&cache_dir.display().to_string())
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::paths::create_dir_owner_only(cache_dir)
                .context("cache directory not writable")?;
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "stat cache directory {}",
                crate::util::display_safe(&cache_dir.display().to_string())
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // A link at the cache folder is refused. The probe would otherwise land in the link's target
    // (#3116), so the target must stay empty.
    #[cfg(unix)]
    #[test]
    fn ensure_cache_dir_writable_refuses_a_link_and_leaves_its_target_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("cache");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(ensure_cache_dir_writable(&link).is_err());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    }
}
