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
    let staging = cache_root.join(format!("{hash}.tmp"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
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
            // Another concurrent writer may have completed the same hash. If
            // the destination now exists, accept it and drop our staging dir.
            if dest.exists() {
                let _ = std::fs::remove_dir_all(&staging);
                Ok(dest)
            } else {
                Err(e.into())
            }
        }
    }
}
