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
    let folder = cache::folder_name(&hash);
    let dest = cache_root.join(&folder);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_root)?;

    // Per-call staging directory: `<folder>.<pid>.<nanos>.tmp`. Each concurrent
    // writer gets its own staging path, so they cannot clobber each other on
    // the way in. GC sweeps anything ending in `.tmp` regardless of age.
    let staging = cache_root.join(format!(
        "{folder}.{pid}.{nanos}.tmp",
        pid = std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&staging)?;
    // Rules text (m.agents_md) is rendered by the per-agent adapter under its
    // native filename (CLAUDE.md, AGENTS.md, etc.) — not written here.
    for (rel, abs) in &m.files {
        if crate::paths::has_parent_component(rel.to_string_lossy().as_ref()) {
            anyhow::bail!("path traversal in bundle file: {}", rel.display());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::MergedManifest;
    use std::collections::BTreeMap;

    /// #149: a bundle file with a `..` component must be rejected, not joined
    /// into staging (which would escape the cache dir).
    #[test]
    fn materialize_rejects_path_traversal_in_files() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"x").expect("write src");
        let cache = tmp.path().join("cache");

        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("../escape.txt"), src);
        let m = MergedManifest {
            files,
            ..Default::default()
        };
        let err = materialize(&m, &cache).expect_err("must reject traversal");
        assert!(
            err.to_string().contains("traversal"),
            "unexpected error: {err}"
        );
    }
}
