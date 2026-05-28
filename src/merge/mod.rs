pub mod agents_md;
pub mod capabilities;
pub mod rules;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Capabilities;
use crate::mcp::resolve::ResolvedMcp;
use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};
use capabilities::{CapabilityContributor, merge_capabilities};
use rules::RuleFile;

#[derive(Debug, Clone)]
pub struct BundleRef {
    pub name: String,
    pub path: PathBuf,
    /// Scope-precedence rank for scalar capability resolution (higher wins).
    /// Bundles selected by higher-precedence scopes get a higher rank; the
    /// top-level config outranks every bundle.
    pub precedence: u8,
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
    /// MCP servers resolved for this host, in declaration order. Adapters
    /// render these into the agent-native MCP config (e.g. `mcp.json`). Empty
    /// means no MCP integration is materialized.
    pub mcps: Vec<ResolvedMcp>,
    /// Plugins resolved for this host, deduplicated, in stable order. Adapters
    /// that support plugins render these into the agent-native plugin config.
    /// Empty means no plugin integration is materialized.
    pub plugins: Vec<ResolvedPlugin>,
    /// Marketplaces referenced by `plugins`, with their synced install location
    /// and content token. Rendered into the agent-native marketplace registry.
    pub marketplaces: Vec<ResolvedMarketplace>,
    /// Engine-agnostic capabilities (permissions, hooks, plugins) merged across
    /// the top-level config and every selected bundle's `bundle.yaml`, by value
    /// shape. Adapters translate these into engine-native config.
    pub capabilities: Capabilities,
    /// Per-engine opaque passthrough values (e.g. `claude_code: {alwaysThinkingEnabled: true}`).
    /// These are merged verbatim into the engine's native config by adapters.
    /// Source: top-level `config.yaml` `native:` block.
    pub native: std::collections::BTreeMap<String, serde_yaml::Value>,
}

const COPIED_SUBDIRS: &[&str] = &["skills", "plugins", "hooks"];

/// Top-level config capabilities outrank every bundle. Bundle precedence comes
/// from the selecting scope kind and is always below this.
const TOP_LEVEL_PRECEDENCE: u8 = u8::MAX;

pub fn merge(
    top_level: &Capabilities,
    native: &BTreeMap<String, serde_yaml::Value>,
    bundles: &[BundleRef],
) -> anyhow::Result<MergedManifest> {
    let mut agents_parts = Vec::new();
    let mut files = BTreeMap::new();
    let mut rule_files: Vec<RuleFile> = Vec::new();
    let mut contributors: Vec<CapabilityContributor> = Vec::new();

    if !top_level.is_empty() {
        contributors.push(CapabilityContributor {
            name: "config.yaml".to_string(),
            precedence: TOP_LEVEL_PRECEDENCE,
            capabilities: top_level.clone(),
        });
    }

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

        if let Some(caps) = read_bundle_yaml(&b.path, &b.name)? {
            contributors.push(CapabilityContributor {
                name: format!("bundle '{}'", b.name),
                precedence: b.precedence,
                capabilities: caps,
            });
        }
    }

    Ok(MergedManifest {
        agents_md: agents_md::concat(&agents_parts),
        files,
        rules: rule_files,
        native: native.clone(),
        capabilities: merge_capabilities(&contributors)?,
        ..MergedManifest::default()
    })
}

/// Read an optional `bundle.yaml` capability fragment from a bundle directory.
/// Returns `None` when the file is absent — bundles carry capabilities only if
/// they choose to.
fn read_bundle_yaml(bundle_root: &Path, name: &str) -> anyhow::Result<Option<Capabilities>> {
    let path = bundle_root.join("bundle.yaml");
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("bundle '{name}': reading {}: {e}", path.display()))?;
    let mut caps: Capabilities = serde_yaml::from_str(&s)
        .map_err(|e| anyhow::anyhow!("bundle '{name}': parsing {}: {e}", path.display()))?;

    // Track which bundle each hook came from, so the adapter can resolve relative paths later.
    // We don't resolve paths here because duplicate hooks (e.g., "hooks/guard.sh" from two bundles)
    // must dedup correctly before being adapted into settings.json.
    for hook in &mut caps.hooks {
        hook.bundle_origin = Some(bundle_root.to_path_buf());
    }

    Ok(Some(caps))
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
