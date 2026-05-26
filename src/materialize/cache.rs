use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::merge::MergedManifest;

/// Stable SHA-256 of the merged manifest. Each field is length-prefixed
/// (little-endian u64) before its bytes so concatenation cannot ambiguate
/// boundaries — i.e. `{agents_md="ABC", files={"DE":"FG"}}` and
/// `{agents_md="ABCD", files={"E":"FG"}}` must hash differently.
/// Filesystem-safe version tag baked in by `build.rs`. Format:
/// `{pkg_version}-{git_short_hash}` (or bare `{pkg_version}` when built outside
/// a git checkout). No `-dirty` suffix — all dev builds at a given HEAD share
/// a bucket so iterating doesn't fragment the cache.
///
/// Used as the *prefix* of the materialized folder name (not mixed into the
/// content hash) so manual cleanup is obvious: `ls ~/.cache/llmenv/claude-code`
/// groups folders by binary version, and pruning means removing anything not
/// starting with the current tag.
pub const VERSION_TAG: &str = env!("LLMENV_VERSION_TAG");

/// Compose the on-disk folder name: `{VERSION_TAG}-{content_hash}`. Splitting
/// the version off the content hash keeps the hash a function of inputs only,
/// so two folders that differ in version prefix but share the same content
/// hash are byte-identical — useful for diffing across upgrades.
#[must_use]
pub fn folder_name(content_hash: &str) -> String {
    format!("{VERSION_TAG}-{content_hash}")
}

pub fn hash_manifest(m: &MergedManifest) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    update_len_prefixed(&mut h, m.agents_md.as_bytes());
    h.update((m.files.len() as u64).to_le_bytes());
    for (rel, abs) in &m.files {
        let rel_str = rel.to_string_lossy();
        update_len_prefixed(&mut h, rel_str.as_bytes());
        let bytes = std::fs::read(abs)?;
        update_len_prefixed(&mut h, &bytes);
    }
    // Mix in rules so adding/editing a `rules/*.md` invalidates the cache.
    // Hash the raw text — covers both frontmatter and body without needing
    // a second pass and matches what gets written to disk for Claude.
    h.update((m.rules.len() as u64).to_le_bytes());
    for r in &m.rules {
        update_len_prefixed(&mut h, r.bundle.as_bytes());
        let rel_str = r.rel.to_string_lossy();
        update_len_prefixed(&mut h, rel_str.as_bytes());
        update_len_prefixed(&mut h, r.raw.as_bytes());
    }
    // Mix in ICM config so a change in MCP wiring invalidates the cache.
    // Serialize as JSON for a deterministic byte representation.
    let icm_bytes = serde_json::to_vec(&m.icm)?;
    update_len_prefixed(&mut h, &icm_bytes);
    h.update([u8::from(m.icm_is_server)]);
    Ok(hex::encode(h.finalize()))
}

fn update_len_prefixed(h: &mut Sha256, data: &[u8]) {
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub removed: Vec<PathBuf>,
    pub kept: usize,
}

/// Selects which cache folders `prune` targets.
#[derive(Debug, Clone, Copy)]
pub enum PruneMode {
    /// Remove every cache folder unconditionally.
    All,
    /// Remove current-version folders whose newest mtime is older than this.
    OlderThan(Duration),
    /// Remove only stale (version-mismatched) folders. `*.tmp` always go.
    StaleOnly,
}

#[derive(Debug, Default)]
pub struct PruneReport {
    pub removed: Vec<PathBuf>,
    pub kept: usize,
}

/// Prune cache folders under `cache_root` according to `mode`.
///
/// Behavior:
/// - `*.tmp` staging dirs are always removed (orphaned partial writes).
/// - `StaleOnly`: removes folders whose name prefix != current `VERSION_TAG`.
/// - `OlderThan(d)`: removes current-version folders older than `d`.
/// - `All`: removes every folder unconditionally.
///
/// Security invariants:
/// - Only direct children of `cache_root` are considered; the walk never
///   recurses across symlinks, so a symlinked entry is unlinked (link only,
///   never its target) rather than followed.
/// - When `dry_run` is true, zero filesystem mutations occur — the report
///   lists what *would* be removed.
///
/// # Errors
/// Returns an error if reading `cache_root` or removing an entry fails.
pub fn prune(cache_root: &Path, mode: PruneMode, dry_run: bool) -> anyhow::Result<PruneReport> {
    let mut report = PruneReport::default();
    if !cache_root.exists() {
        return Ok(report);
    }
    let now = SystemTime::now();
    for entry in std::fs::read_dir(cache_root)? {
        let entry = entry?;
        let p = entry.path();
        // lstat-equivalent: a symlink at the top level is never followed. We
        // remove the link itself (never its target) so the cache root can't be
        // used to delete arbitrary files outside it.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            remove_link(&p, dry_run, &mut report);
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        // Orphaned staging dirs are always removed regardless of mode.
        if p.extension().is_some_and(|e| e == "tmp") {
            remove_dir(&p, dry_run, &mut report)?;
            continue;
        }

        let is_current = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&format!("{VERSION_TAG}-")));

        let should_remove = match mode {
            PruneMode::All => true,
            PruneMode::StaleOnly => !is_current,
            PruneMode::OlderThan(older_than) => {
                // Only current-version folders are aged out; stale folders are
                // left to a StaleOnly/All pass so the two axes stay orthogonal.
                if is_current {
                    let m = newest_mtime(&p)?;
                    now.duration_since(m).unwrap_or_default() > older_than
                } else {
                    false
                }
            }
        };

        if should_remove {
            remove_dir(&p, dry_run, &mut report)?;
        } else {
            report.kept += 1;
        }
    }
    Ok(report)
}

/// Record a directory removal, performing the unlink unless `dry_run`.
fn remove_dir(p: &Path, dry_run: bool, report: &mut PruneReport) -> anyhow::Result<()> {
    if !dry_run {
        std::fs::remove_dir_all(p)?;
    }
    report.removed.push(p.to_path_buf());
    Ok(())
}

/// Record a symlink removal, performing the unlink unless `dry_run`.
fn remove_link(p: &Path, dry_run: bool, report: &mut PruneReport) {
    if !dry_run {
        // A failed unlink here is non-fatal: report what we attempted and
        // continue pruning the rest of the cache root.
        let _ = std::fs::remove_file(p);
    }
    report.removed.push(p.to_path_buf());
}

/// Remove cache subdirectories whose newest mtime is older than `older_than`.
/// `*.tmp` staging directories are removed regardless of age — they represent
/// orphaned partial writes from a previous crashed `materialize` call.
pub fn gc(cache_root: &Path, older_than: Duration) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    if !cache_root.exists() {
        return Ok(report);
    }
    let now = SystemTime::now();
    for entry in std::fs::read_dir(cache_root)? {
        let entry = entry?;
        let p = entry.path();
        // Use `file_type()` (lstat-equivalent) — a symlink at the top level
        // is never treated as a cache directory we own. Removing one removes
        // only the link, never the target.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            std::fs::remove_file(&p)?;
            report.removed.push(p);
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        if p.extension().is_some_and(|e| e == "tmp") {
            std::fs::remove_dir_all(&p)?;
            report.removed.push(p);
            continue;
        }
        let m = newest_mtime(&p)?;
        if now.duration_since(m).unwrap_or_default() > older_than {
            std::fs::remove_dir_all(&p)?;
            report.removed.push(p);
        } else {
            report.kept += 1;
        }
    }
    Ok(report)
}

/// Newest mtime found anywhere under `dir` (including the dir itself).
fn newest_mtime(dir: &Path) -> anyhow::Result<SystemTime> {
    let mut newest = dir.metadata()?.modified()?;
    walk_mtime(dir, &mut newest)?;
    Ok(newest)
}

fn walk_mtime(dir: &Path, newest: &mut SystemTime) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let m = entry.metadata()?.modified()?;
        if m > *newest {
            *newest = m;
        }
        if file_type.is_dir() {
            walk_mtime(&entry.path(), newest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch_dir(root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("file.txt"), b"x").unwrap();
        p
    }

    #[test]
    fn prune_missing_root_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report = prune(&missing, PruneMode::All, false).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.kept, 0);
    }

    #[test]
    fn prune_all_removes_every_folder() {
        let tmp = tempfile::tempdir().unwrap();
        touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::All, false).unwrap();
        assert_eq!(report.removed.len(), 2);
        assert_eq!(report.kept, 0);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn prune_stale_only_keeps_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let current = touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        let stale = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::StaleOnly, false).unwrap();
        assert_eq!(report.kept, 1);
        assert!(report.removed.contains(&stale));
        assert!(!report.removed.contains(&current));
        assert!(current.exists());
        assert!(!stale.exists());
    }

    #[test]
    fn prune_always_removes_tmp_staging_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = touch_dir(tmp.path(), &format!("{VERSION_TAG}-cccc.tmp"));
        let report = prune(tmp.path(), PruneMode::StaleOnly, false).unwrap();
        assert!(report.removed.contains(&staging));
        assert!(!staging.exists());
    }

    #[test]
    fn prune_dry_run_mutates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        let b = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::All, true).unwrap();
        // Reports what *would* be removed, but leaves the filesystem intact.
        assert_eq!(report.removed.len(), 2);
        assert!(a.exists());
        assert!(b.exists());
    }

    #[test]
    fn prune_older_than_skips_stale_folders() {
        let tmp = tempfile::tempdir().unwrap();
        // A stale folder is NOT aged out by OlderThan — only current-version
        // folders are subject to the age check, keeping the two axes orthogonal.
        let stale = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::OlderThan(Duration::ZERO), false).unwrap();
        // Current-version dir absent; stale kept because OlderThan ignores it.
        assert!(stale.exists());
        assert_eq!(report.kept, 1);
        assert!(report.removed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn prune_unlinks_symlink_without_following() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        // Target lives OUTSIDE the cache root; pruning must not touch it.
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"important").unwrap();

        let cache_root = tmp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let link = cache_root.join("link");
        symlink(&outside, &link).unwrap();

        let report = prune(&cache_root, PruneMode::All, false).unwrap();
        assert!(report.removed.contains(&link));
        // The link is gone; the target and its contents survive.
        assert!(!link.exists());
        assert!(outside.join("keep.txt").exists());
    }
}
