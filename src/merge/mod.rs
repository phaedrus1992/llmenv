pub mod agents_md;
pub mod rules;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Icm;
use rules::RuleFile;

#[derive(Debug, Clone)]
pub struct BundleRef {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct MergedManifest {
    /// Concatenated AGENTS.md with `<!-- # from bundle: <name> -->` provenance separators.
    pub agents_md: String,
    /// Relative path inside the bundle → absolute source path. Later bundles
    /// overwrite earlier ones on path collision.
    pub files: BTreeMap<PathBuf, PathBuf>,
    /// Per-bundle `rules/*.md` ingested with frontmatter split out. Adapters
    /// choose between writing them as separate files (Claude) or appending
    /// the bodies into AGENTS.md (fallback). Stored in the order rules were
    /// collected: bundles in declaration order, files within a bundle sorted
    /// by relative path.
    pub rules: Vec<RuleFile>,
    /// ICM configuration to emit into agent-native MCP config. `None` means
    /// no ICM integration is materialized.
    pub icm: Option<Icm>,
    /// True when this host's active scope tags include `icm.server_tag` — the
    /// adapter emits a local stdio MCP entry and the hook ensures `mcp-proxy`
    /// is running. False means register an HTTP client pointing at `icm.client_url`.
    pub icm_is_server: bool,
}

const COPIED_SUBDIRS: &[&str] = &["skills", "plugins", "hooks"];

pub fn merge(bundles: &[BundleRef]) -> anyhow::Result<MergedManifest> {
    let mut agents_parts = Vec::new();
    let mut files = BTreeMap::new();
    let mut rule_files: Vec<RuleFile> = Vec::new();
    for b in bundles {
        let am = b.path.join("AGENTS.md");
        if am.exists() {
            agents_parts.push((b.name.clone(), std::fs::read_to_string(&am)?));
        }
        for sub in COPIED_SUBDIRS {
            let dir = b.path.join(sub);
            if !dir.exists() {
                continue;
            }
            walk(&b.path, &dir, &mut files)?;
        }
        rule_files.extend(rules::collect_from_bundle(&b.path, &b.name)?);
    }
    Ok(MergedManifest {
        agents_md: agents_md::concat(&agents_parts),
        files,
        rules: rule_files,
        ..MergedManifest::default()
    })
}

/// Walk `dir` collecting regular files into `out`, keyed by their path
/// relative to `bundle_root`. Symlinks are skipped.
fn walk(
    bundle_root: &Path,
    dir: &Path,
    out: &mut BTreeMap<PathBuf, PathBuf>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let p = entry.path();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk(bundle_root, &p, out)?;
        } else if file_type.is_file() {
            let rel = p
                .strip_prefix(bundle_root)
                .map_err(|e| anyhow::anyhow!("path {} not under bundle root: {e}", p.display()))?
                .to_path_buf();
            out.insert(rel, p);
        }
    }
    Ok(())
}
