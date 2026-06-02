pub mod cache;
pub mod manifest;
pub mod state;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::config::HashingMode;
use crate::merge::MergedManifest;

/// Outcome of [`materialize`]: the folder llmenv rendered into, plus the
/// content hash it rendered (so callers can record it in the dotfile without
/// re-hashing).
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The materialized folder (`<cache_root>/<adapter>/<folder_name>`).
    pub path: PathBuf,
    /// The content hash of `m` (the [`cache::hash_manifest`] result).
    pub hash: String,
}

/// Materialize the bundle files of `m` into a subdirectory of `cache_root`,
/// named per the active [`HashingMode`] (#246).
///
/// - [`HashingMode::Loose`]: folder = `<shape>`. Selection-addressed, version
///   agnostic; written in place (folder reused across content edits + upgrades).
/// - [`HashingMode::Normal`]: folder = `<version_mm>/<shape>`. Reused across
///   content edits within a `major.minor` generation; written in place.
/// - [`HashingMode::Strict`]: folder = `{VERSION_TAG}-{hash}`. Writes are staged
///   to a per-call `.tmp/` dir and atomically renamed into place; an existing
///   destination is a no-op (byte-identical by construction).
///
/// Loose/normal write in place (no staging swap) because the folder is the
/// agent's live config dir for the whole session — a swap would destroy foreign
/// in-session state (#175). Stale-file reconciliation against the owned-set
/// manifest happens in the orchestrator after the adapter runs.
///
/// This function only handles `m.files` (raw bundle content). The agent adapter
/// writes the native files (CLAUDE.md, settings.json, …) on top, and the
/// orchestrator records the combined owned set + content hash in the dotfile.
pub fn materialize(m: &MergedManifest, cache_root: &Path) -> anyhow::Result<Rendered> {
    let shape = cache::shape(&BTreeSet::new(), &BTreeSet::new());
    materialize_with_mode(m, cache_root, HashingMode::default(), &shape)
}

/// [`materialize`] with an explicit mode + selection `shape`. `materialize` is
/// the default-mode, empty-selection convenience wrapper used by tests and
/// callers that don't thread config through.
pub fn materialize_with_mode(
    m: &MergedManifest,
    cache_root: &Path,
    mode: HashingMode,
    shape: &str,
) -> anyhow::Result<Rendered> {
    let hash = cache::hash_manifest(m)?;
    let folder = cache::folder_name(mode, shape, &hash);
    let dest = cache_root.join(&folder);

    match mode {
        // Loose/normal reuse one folder across content edits: write in place,
        // never swap (the folder is the agent's live home). Stale-file cleanup
        // is the orchestrator's job via the owned-set manifest.
        HashingMode::Loose | HashingMode::Normal => {
            write_in_place(m, &dest)?;
            return Ok(Rendered { path: dest, hash });
        }
        // Strict mode: a content-hashed folder that already exists is
        // byte-identical, so reuse it untouched.
        HashingMode::Strict if dest.exists() => {
            return Ok(Rendered { path: dest, hash });
        }
        HashingMode::Strict => {}
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
        if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
            anyhow::bail!("path traversal in bundle file: {}", rel.display());
        }
        let out = staging.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(abs, &out)?;
    }
    match std::fs::rename(&staging, &dest) {
        Ok(()) => Ok(Rendered { path: dest, hash }),
        Err(e) => {
            // Another concurrent writer raced us to the same hash. Their dir
            // is byte-identical (same hash ⇒ same contents), so accept it
            // and drop our staging.
            if dest.exists() {
                let _ = std::fs::remove_dir_all(&staging);
                Ok(Rendered { path: dest, hash })
            } else {
                let _ = std::fs::remove_dir_all(&staging);
                Err(e.into())
            }
        }
    }
}

/// Copy `m.files` into `dest` in place (loose/normal mode). No staging swap:
/// `dest` is the agent's live config dir, so foreign in-session files survive.
/// Stale llmenv-owned files from a prior render are reconciled separately by
/// the orchestrator against the owned-set manifest — this function only writes
/// the current content. Idempotent: re-copying the same bytes is harmless.
///
/// If `m.files` is empty, `dest` is not created (skip empty directories). If
/// `dest` already exists but becomes empty after reconciliation, it will be
/// cleaned up by the adapter or orchestrator's owned-set reconciliation.
fn write_in_place(m: &MergedManifest, dest: &Path) -> anyhow::Result<()> {
    if m.files.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dest)?;
    for (rel, abs) in &m.files {
        if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
            anyhow::bail!("path traversal in bundle file: {}", rel.display());
        }
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(abs, &out)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

    /// #149: an absolute `rel` would escape staging via Path::join's
    /// "absolute argument discards base" rule. Must be rejected.
    #[test]
    fn materialize_rejects_absolute_path_in_files() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, b"x").expect("write src");
        let cache = tmp.path().join("cache");

        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("/etc/llmenv-escape.txt"), src);
        let m = MergedManifest {
            files,
            ..Default::default()
        };
        let err = materialize(&m, &cache).expect_err("must reject absolute path");
        assert!(
            err.to_string().contains("traversal"),
            "unexpected error: {err}"
        );
    }
}
