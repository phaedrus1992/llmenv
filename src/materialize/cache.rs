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
