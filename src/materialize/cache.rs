use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::merge::MergedManifest;

/// Stable SHA-256 of the merged manifest: `agents_md` followed by each
/// `(relative_path, file_contents)` in `BTreeMap` iteration order.
pub fn hash_manifest(m: &MergedManifest) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    h.update(m.agents_md.as_bytes());
    for (rel, abs) in &m.files {
        h.update(rel.to_string_lossy().as_bytes());
        let bytes = std::fs::read(abs)?;
        h.update(&bytes);
    }
    Ok(hex::encode(h.finalize()))
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
        if !entry.file_type()?.is_dir() {
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
