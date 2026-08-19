pub mod cache;
pub(crate) mod inherit;
pub mod manifest;
pub(crate) mod merge_cache;
pub(crate) mod schema_gen;
pub mod state;
mod status_data;

pub(crate) use status_data::{ConfigStaleInputs, collect_status_data};

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
/// - [`HashingMode::Normal`]: folder = `<version_major>/<shape>`. Reused across
///   content edits within a major-version generation; written in place.
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
    let cache_start = std::time::Instant::now();
    let hash = cache::hash_manifest(m)?;
    let folder = cache::folder_name(mode, shape, &hash);
    let dest = cache_root.join(&folder);

    // Owner-only (#1198): hardens/self-heals `cache_root` itself before any
    // mode-specific branch, including the early-return fast paths below —
    // create_dir_owner_only's self-heal only touches the exact path it's
    // called on, so calling it later (or only on `dest`, a descendant) would
    // leave a *pre-existing* cache_root from an older llmenv (created via
    // bare create_dir_all) world-readable forever.
    crate::paths::create_dir_owner_only(cache_root)?;

    match mode {
        // Loose/normal reuse one folder across content edits: write in place,
        // never swap (the folder is the agent's live home). Stale-file cleanup
        // is the orchestrator's job via the owned-set manifest. Not part of
        // the content_hash cache's hit/miss telemetry (#1260): every call
        // writes, there's no hit/miss distinction to report.
        HashingMode::Loose | HashingMode::Normal => {
            write_in_place(m, &dest)?;
            return Ok(Rendered { path: dest, hash });
        }
        // Strict mode: a content-hashed folder that already exists is
        // byte-identical, so reuse it untouched.
        HashingMode::Strict if dest.exists() => {
            crate::cache_trace::emit_cache_trace("content_hash", true, cache_start.elapsed(), None);
            return Ok(Rendered { path: dest, hash });
        }
        HashingMode::Strict => {}
    }
    crate::cache_trace::emit_cache_trace("content_hash", false, cache_start.elapsed(), None);

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
    // Owner-only (#1198): the Strict-mode path was hardened in #1196, but
    // Loose/Normal — llmenv's *default* mode — took this separate code path
    // and was left unprotected. `cache_root` itself is now hardened by the
    // caller (materialize_with_mode) before reaching here; this call
    // additionally hardens `dest` and anything between it and `cache_root`.
    // Files/subdirs written inside `dest` stay plain — contained by the
    // hardened tree above them, same as materialize_with_mode's own Strict
    // staging dir (created plain, since it's nested under the now-always-
    // hardened `cache_root`).
    crate::paths::create_dir_owner_only(dest)?;
    for (rel, abs) in &m.files {
        if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
            anyhow::bail!("path traversal in bundle file: {}", rel.display());
        }
        write_bundle_file(dest, rel, abs)
            .with_context(|| format!("writing bundle file {}", rel.display()))?;
    }
    prune_empty_dirs(dest)?;
    Ok(())
}

/// Write one `m.files` entry into `dest`, replacing whatever is at the
/// destination — including a symlinked leaf or a symlinked *directory*
/// anywhere along `rel` — rather than writing through it.
///
/// `dest` persists across renders (the agent's live config dir), so a prior
/// render's output could have been replaced by a symlink between calls.
/// `create_dir_all` plus a leaf-only guard (`copy_replacing_symlink`) closes
/// that for the final component but still resolves every *directory*
/// component of `rel` by path, following a symlink planted at any of them —
/// a strictly stronger primitive than the leaf case (#1341-class TOCTOU,
/// #1423, extended to directory components in #1427).
/// `write_file_through_dirs` walks each component via `openat`-relative
/// descent instead, so no intermediate symlink is ever followed.
fn write_bundle_file(dest: &Path, rel: &Path, abs: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
    let mode = bundle_file_mode(abs)?;
    crate::paths::dirfd::write_file_through_dirs(dest, rel, &bytes, mode)
        .map_err(anyhow::Error::from)
}

/// The mode to write a copied bundle file with: the source's mode with
/// group/other write and setuid/setgid masked off (`& 0o755`) — full mode
/// propagation would let a bundle file sourced from a permissive local path
/// or tarball land group/world-writable inside a directory the agent
/// executes code from, the same reasoning `copy_replacing_symlink` masks for
/// (security-audit, #1426).
#[cfg(unix)]
pub(crate) fn bundle_file_mode(abs: &Path) -> anyhow::Result<rustix::fs::Mode> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(abs)
        .with_context(|| format!("reading metadata for {}", abs.display()))?
        .permissions()
        .mode();
    // `RawMode`, not a hardcoded integer width: `Mode`'s underlying bits type
    // is `u16` on macOS/BSD but `u32` on Linux, and `RawMode` is rustix's own
    // portable alias for it (matching how `dirfd.rs`'s `FileType::from_raw_mode`
    // already does this cross-platform conversion). `mode & 0o755` is bounded
    // well within either width, so this never truncates real bits —
    // `from_bits_truncate` still guards against a stray high bit `Mode`
    // doesn't model rather than panicking on one.
    let masked = rustix::fs::RawMode::try_from(mode & 0o755).unwrap_or(0o644);
    Ok(rustix::fs::Mode::from_bits_truncate(masked))
}

#[cfg(not(unix))]
pub(crate) fn bundle_file_mode(_abs: &Path) -> anyhow::Result<rustix::fs::Mode> {
    Ok(rustix::fs::Mode::from(0o644))
}

/// Remove empty directories under `root` (excluding `root` itself), walking
/// bottom-up so child dirs are pruned before their parents. Called after each
/// render pass to clean up dirs from bundles that contributed no files (#336).
///
/// Per-entry errors are non-fatal: a leftover empty dir is cosmetically bad
/// but not a correctness failure.
pub(crate) fn prune_empty_dirs(root: &Path) -> anyhow::Result<()> {
    use std::os::fd::AsFd as _;

    // #1066: `root` is resolved once and the walk descends by file descriptor.
    // The path-based version recursed with `read_dir` on each path it had just
    // stat'd, so an intermediate directory swapped for a symlink redirected
    // the walk — and this walk *removes* directories, so a redirected one
    // deletes empty directories somewhere the caller never named.
    let dir = match crate::paths::dirfd::open_dir_nofollow(root) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::anyhow!("reading directory {}: {e}", root.display())),
    };
    prune_empty_dirs_at(dir.as_fd(), root);
    Ok(())
}

/// Remove empty directories under an already-open `dir`, bottom-up. `root` is
/// never removed — only entries beneath it — which is what the caller relies
/// on when pruning a tree it still intends to use.
///
/// Failures are warned rather than propagated, matching the previous
/// behaviour: pruning is opportunistic tidying, and a directory that can't be
/// read or removed is not a reason to fail the export around it.
fn prune_empty_dirs_at(dir: std::os::fd::BorrowedFd<'_>, at: &Path) {
    use std::os::fd::AsFd as _;

    let entries = match crate::paths::dirfd::read_dir_entries(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("prune_empty_dirs: could not read {}: {e}", at.display());
            return;
        }
    };
    for entry in entries {
        if !entry.is_dir() {
            continue;
        }
        let child_at = at.join(&entry.name);
        let child = match crate::paths::dirfd::open_dir_at(dir, &entry.name) {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!(
                    "prune_empty_dirs: could not read {}: {e}",
                    child_at.display()
                );
                continue;
            }
        };
        prune_empty_dirs_at(child.as_fd(), &child_at);
        // Re-read rather than tracking a count: the recursion above may have
        // emptied it, and something else may have filled it.
        let now_empty = crate::paths::dirfd::read_dir_entries(child.as_fd())
            .is_ok_and(|remaining| remaining.is_empty());
        if now_empty && let Err(e) = crate::paths::dirfd::remove_dir_at(dir, &entry.name) {
            tracing::warn!(
                "prune_empty_dirs: could not remove {}: {e}",
                child_at.display()
            );
        }
    }
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

    /// `write_in_place` (loose/normal mode) re-renders `m.files` into a
    /// folder that persists across calls — the agent's live config dir. If a
    /// prior render's destination entry gets replaced by a symlink (e.g. a
    /// malicious or misbehaving plugin), the next render must replace the
    /// symlink rather than write through it, matching the hardening
    /// `src/materialize/inherit.rs` already applies for the same class of
    /// TOCTOU bug (#1341, extended here per #1423).
    #[cfg(unix)]
    #[test]
    fn write_in_place_does_not_write_through_a_symlinked_destination() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("bundle-file.txt");
        std::fs::write(&src, b"bundle-content").expect("write src");
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("create cache");

        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, b"must-not-be-touched").expect("write victim");

        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("out.txt"), src);
        let m = MergedManifest {
            files,
            ..Default::default()
        };

        let rendered = materialize(&m, &cache).expect("first render");
        let dest_file = rendered.path.join("out.txt");
        std::fs::remove_file(&dest_file).expect("remove first render's output");
        std::os::unix::fs::symlink(&victim, &dest_file).expect("plant symlink");

        materialize(&m, &cache).expect("second render");

        assert!(
            !std::fs::symlink_metadata(&dest_file)
                .expect("stat dest")
                .file_type()
                .is_symlink(),
            "the planted symlink must be replaced, not written through"
        );
        assert_eq!(
            std::fs::read(&dest_file).expect("read dest"),
            b"bundle-content"
        );
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"must-not-be-touched",
            "the symlink's target must be untouched"
        );
    }

    /// The stronger case #1427 closes: a symlinked *directory* component
    /// anywhere in a bundle file's relative path must not be followed either
    /// — `create_dir_all` on a path containing one would happily resolve
    /// through it, landing the write inside the symlink's target instead of
    /// under `dest`.
    #[cfg(unix)]
    #[test]
    fn write_in_place_does_not_follow_a_symlinked_directory_component() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("bundle-file.txt");
        std::fs::write(&src, b"bundle-content").expect("write src");
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("create cache");

        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create outside");

        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("hooks/out.txt"), src);
        let m = MergedManifest {
            files,
            ..Default::default()
        };

        // First render creates `hooks/` for real; swap it for a symlink to a
        // directory outside `dest` before the next render.
        let rendered = materialize(&m, &cache).expect("first render");
        let hooks_dir = rendered.path.join("hooks");
        std::fs::remove_dir_all(&hooks_dir).expect("remove first render's hooks dir");
        std::os::unix::fs::symlink(&outside, &hooks_dir).expect("plant symlinked directory");

        let err = materialize(&m, &cache).expect_err("must refuse to follow the symlinked dir");
        let _ = err;

        assert!(
            !outside.join("out.txt").exists(),
            "the write must not land inside the symlinked directory's target"
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
