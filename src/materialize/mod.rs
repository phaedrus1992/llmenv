pub mod cache;
pub mod manifest;
pub mod state;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

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
    let folder = cache::folder_name(&mode, shape, &hash);
    let dest = cache_root.join(&folder);

    match mode {
        // Loose/normal reuse one folder across content edits: write in place,
        // never swap (the folder is the agent's live home). Stale-file cleanup
        // is the orchestrator's job via the owned-set manifest.
        HashingMode::Loose | HashingMode::Normal { .. } => {
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
    prune_empty_dirs(dest)?;
    Ok(())
}

/// Remove empty directories under `root` (excluding `root` itself), walking
/// bottom-up so child dirs are pruned before their parents. Called after each
/// render pass to clean up dirs from bundles that contributed no files (#336).
///
/// Per-entry errors are non-fatal: a leftover empty dir is cosmetically bad
/// but not a correctness failure.
pub(crate) fn prune_empty_dirs(root: &Path) -> anyhow::Result<()> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    collect_subdirs(root, &mut dirs)?;
    dirs.reverse();
    for dir in dirs {
        let is_empty = match std::fs::read_dir(&dir) {
            Ok(mut rd) => rd.next().is_none(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!("prune_empty_dirs: could not read {}: {e}", dir.display());
                continue;
            }
        };
        if is_empty && let Err(e) = std::fs::remove_dir(&dir) {
            tracing::warn!("prune_empty_dirs: could not remove {}: {e}", dir.display());
        }
    }
    Ok(())
}

/// Recursively collect every subdirectory under `dir` in depth-first pre-order.
/// The caller reverses for bottom-up traversal.
fn collect_subdirs(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::anyhow!("reading directory {}: {e}", dir.display())),
    };
    for entry in rd {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        // DirEntry::metadata() uses lstat on Unix — does not follow symlinks,
        // so symlinks to directories outside the render root are not traversed.
        let path = entry.path();
        let meta = entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        if meta.is_dir() {
            out.push(path.clone());
            collect_subdirs(&path, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::merge::MergedManifest;
    use proptest::prelude::*;
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

    // #341: prune_empty_dirs — root is never removed regardless of tree shape.
    proptest! {
        #[test]
        fn prune_empty_dirs_never_removes_root(
            dirs in proptest::collection::vec("[a-z]{1,6}", 0..8_usize)
        ) {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let root = tmp.path().join("out");
            std::fs::create_dir_all(&root).expect("create root");
            for d in &dirs {
                std::fs::create_dir_all(root.join(d)).expect("create subdir");
            }
            prune_empty_dirs(&root).expect("prune");
            prop_assert!(root.exists(), "root must survive prune");
        }
    }

    // #341: prune_empty_dirs — files in subdirs are preserved.
    proptest! {
        #[test]
        fn prune_empty_dirs_preserves_files(
            dir in "[a-z]{1,6}",
            filename in "[a-z]{1,6}"
        ) {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let root = tmp.path().join("out");
            let subdir = root.join(&dir);
            std::fs::create_dir_all(&subdir).expect("create subdir");
            let file = subdir.join(&filename);
            std::fs::write(&file, b"content").expect("write file");
            prune_empty_dirs(&root).expect("prune");
            prop_assert!(file.exists(), "file must survive prune");
            prop_assert!(subdir.exists(), "non-empty dir must survive prune");
        }
    }

    // #341: prune_empty_dirs — idempotent: second run produces same result.
    proptest! {
        #[test]
        fn prune_empty_dirs_is_idempotent(
            dirs in proptest::collection::vec("[a-z]{1,6}", 0..6_usize)
        ) {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let root = tmp.path().join("out");
            std::fs::create_dir_all(&root).expect("create root");
            for d in &dirs {
                std::fs::create_dir_all(root.join(d)).expect("create subdir");
            }
            prune_empty_dirs(&root).expect("first prune");
            prune_empty_dirs(&root).expect("second prune");
            prop_assert!(root.exists(), "root must still exist after second prune");
        }
    }
}
