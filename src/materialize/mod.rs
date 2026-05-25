pub mod cache;

use std::path::{Path, PathBuf};

use crate::merge::MergedManifest;

/// Materialize `m` into a content-hashed subdirectory of `cache_root`.
///
/// Writes are staged to `<cache_root>/<hash>.tmp/` and atomically renamed to
/// `<cache_root>/<hash>/` on success. If the destination already exists the
/// call is a no-op and the existing path is returned.
pub fn materialize(m: &MergedManifest, cache_root: &Path) -> anyhow::Result<PathBuf> {
    let hash = cache::hash_manifest(m)?;
    let dest = cache_root.join(&hash);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_root)?;

    // Per-call staging directory: `<hash>.<pid>.<nanos>.tmp`. Each concurrent
    // writer gets its own staging path, so they cannot clobber each other on
    // the way in. GC sweeps anything ending in `.tmp` regardless of age.
    let staging = cache_root.join(format!(
        "{hash}.{pid}.{nanos}.tmp",
        pid = std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&staging)?;
    std::fs::write(staging.join("AGENTS.md"), &m.agents_md)?;
    for (rel, abs) in &m.files {
        let out = staging.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(abs, &out)?;
    }
    match std::fs::rename(&staging, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            // Another concurrent writer raced us to the same hash. Their dir
            // is byte-identical (same hash ⇒ same contents), so accept it
            // and drop our staging.
            if dest.exists() {
                let _ = std::fs::remove_dir_all(&staging);
                Ok(dest)
            } else {
                let _ = std::fs::remove_dir_all(&staging);
                Err(e.into())
            }
        }
    }
}
