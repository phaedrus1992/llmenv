pub mod agents_md;
pub mod capabilities;
pub mod rules;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Capabilities;
use crate::mcp::resolve::ResolvedMcp;
use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};
use crate::util::{merge_yaml, normalize_yaml};
pub use capabilities::{CapabilityContributor, merge_capabilities};
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
    /// Sources: top-level `config.yaml` `native:` block (highest precedence) deep-merged
    /// with `native:` blocks from each selected bundle's `bundle.yaml`.
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

    let merged_caps = merge_capabilities(&contributors)?;

    // Merge bundle native: blocks (lower precedence) with the top-level native:
    // block (highest precedence). Start with bundle contributions, then overlay
    // the top-level so it always wins on scalar collisions.
    let mut merged_native = merged_caps.native.clone();
    for (key, value) in native {
        match merged_native.get_mut(key) {
            Some(existing) => merge_yaml(existing, value.clone()),
            None => {
                let mut normalized = value.clone();
                normalize_yaml(&mut normalized);
                merged_native.insert(key.clone(), normalized);
            }
        }
    }

    Ok(MergedManifest {
        agents_md: agents_md::concat(&agents_parts),
        files,
        rules: rule_files,
        native: merged_native,
        capabilities: merged_caps,
        ..MergedManifest::default()
    })
}

/// Keys that a `bundle.yaml` fragment is allowed to declare. Any other top-level
/// key is rejected with a hard error rather than silently dropped.
const BUNDLE_YAML_KNOWN_KEYS: &[&str] = &[
    "permissions",
    "hooks",
    "plugins",
    "mcp",
    "env",
    "auto_memory_enabled",
    "effort_level",
    "advisor_size",
    "native_permissions",
    "native_hooks",
    "native_plugins",
    "native_mcp",
    "native",
    "features",
    "host",
];

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
    let raw: serde_yaml::Value = serde_yaml::from_str(&s)
        .map_err(|e| anyhow::anyhow!("bundle '{name}': parsing {}: {e}", path.display()))?;
    if let Some(mapping) = raw.as_mapping() {
        for key in mapping.keys() {
            if let Some(k) = key.as_str()
                && !BUNDLE_YAML_KNOWN_KEYS.contains(&k)
            {
                anyhow::bail!(
                    "bundle '{name}': unknown key '{k}' in bundle.yaml — \
                     known keys: {}",
                    BUNDLE_YAML_KNOWN_KEYS.join(", ")
                );
            }
        }
    }
    let mut caps: Capabilities = serde_yaml::from_value(raw)
        .map_err(|e| anyhow::anyhow!("bundle '{name}': parsing {}: {e}", path.display()))?;

    // Track which bundle each hook came from, so the adapter can resolve relative paths later.
    // We don't resolve paths here because duplicate hooks (e.g., "hooks/guard.sh" from two bundles)
    // must dedup correctly before being adapted into settings.json.
    for hook in &mut caps.hooks {
        hook.bundle_origin = Some(bundle_root.to_path_buf());
    }

    let context = format!("bundle '{name}'");
    for key in caps.env.keys() {
        if crate::materialize::state::RESERVED_STATE_ENV_VARS.contains(&key.as_str()) {
            anyhow::bail!(
                "{context}: capabilities.env key '{key}' is reserved — it is emitted by the \
                 adapter or state system and must not be overridden here. \
                 Fix: remove this key from env:, or use bundle.vars for template variables."
            );
        }
        if key.starts_with("LLMENV_") {
            anyhow::bail!(
                "{context}: capabilities.env key '{key}' uses the 'LLMENV_' prefix, which is \
                 reserved for llmenv-internal variables. Fix: rename the key."
            );
        }
    }

    // Validate bundle-contributed memory entries with the same checks that
    // Config::validate() applies to top-level features.memory entries.
    if let Some(features) = &caps.features {
        for mem in &features.memory {
            if mem.tags.is_empty() {
                anyhow::bail!(
                    "{context}: features.memory entry for '{}'  has no tags — every memory entry must declare at least one activation tag",
                    mem.server_host
                );
            }
            if mem.listen_host.parse::<std::net::IpAddr>().is_err() {
                anyhow::bail!(
                    "{context}: features.memory entry for '{}': listen_host '{}' is not a valid \
                     IP address literal (hostnames not supported)",
                    mem.server_host,
                    mem.listen_host
                );
            }
        }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    // #329: a bundle.yaml with an mcp: block must contribute to MergedManifest capabilities.mcp.
    #[test]
    fn bundle_mcp_block_appears_in_merged_capabilities() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("mcp-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!("mcp:\n", "  - name: ctx\n", "    command: ctx-mcp\n",),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "mcp-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();

        assert_eq!(
            manifest.capabilities.mcp.len(),
            1,
            "bundle mcp: entry must appear in merged capabilities"
        );
        assert_eq!(manifest.capabilities.mcp[0].name, "ctx");
    }

    // #291: a bundle.yaml with a native: block must contribute to MergedManifest.native.
    #[test]
    fn bundle_native_block_appears_in_merged_output() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("my-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "native:\n  claude_code:\n    statusLine: bundle-value\n",
        )
        .unwrap();

        let bundle = BundleRef {
            name: "my-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();

        assert!(
            manifest.native.contains_key("claude_code"),
            "bundle native: block must appear in MergedManifest.native"
        );
    }

    // Top-level native: must win over bundle native: on scalar collision.
    #[test]
    fn top_level_native_wins_over_bundle_native_on_collision() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "native:\n  claude_code:\n    key: from-bundle\n",
        )
        .unwrap();

        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let mut top_native: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        top_native.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: from-top").unwrap(),
        );

        let manifest = merge(&Capabilities::default(), &top_native, &[bundle]).unwrap();

        let val = manifest.native["claude_code"]
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("key".into())))
            .and_then(serde_yaml::Value::as_str)
            .expect("key must be present");
        assert_eq!(val, "from-top", "top-level native: must win over bundle");
    }

    // Top-level-only native insert must be normalized the same way as a bundle-contributed insert.
    // A sequence value contributed via top-level native: must compare equal (after YAML round-trip)
    // to the same sequence contributed via a bundle, because both paths normalize.
    #[test]
    fn top_level_native_insert_is_normalized() {
        // A sequence contributed only via top-level native: (no bundle collision).
        // After normalize_yaml the sequence tags are stripped, so a round-trip
        // produces the canonical form rather than a tagged representation.
        let mut top_native: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        top_native.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("seq:\n  - one\n  - two\n").unwrap(),
        );

        let manifest = merge(&Capabilities::default(), &top_native, &[]).unwrap();

        let val = manifest
            .native
            .get("claude_code")
            .expect("claude_code key must be present");

        // After normalization the mapping tag must be absent (plain, not tagged).
        let re_serialized = serde_yaml::to_string(val).expect("must serialize");
        assert!(
            !re_serialized.contains("!!"),
            "normalized value must not contain YAML tags: {re_serialized}"
        );
    }

    // #335: a bundle.yaml with a features: block contributes memory entries to merged capabilities.
    #[test]
    fn bundle_features_memory_appears_in_merged_capabilities() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("mem-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  memory:\n",
                "    - server_host: still\n",
                "      port: 9092\n",
                "      tags: [home]\n",
            ),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "mem-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();
        let features = manifest
            .capabilities
            .features
            .as_ref()
            .expect("features must be present");
        assert_eq!(features.memory.len(), 1);
        assert_eq!(features.memory[0].server_host, "still");
    }

    // #335: a bundle.yaml with a host: block contributes host entries to merged capabilities.
    #[test]
    fn bundle_host_block_appears_in_merged_capabilities() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("host-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!("host:\n", "  still:\n", "    addr: still.local\n",),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "host-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();
        assert!(
            manifest.capabilities.host.contains_key("still"),
            "bundle host: entry must appear in merged capabilities"
        );
        assert_eq!(manifest.capabilities.host["still"].addr, "still.local");
    }

    // #335: unknown keys in bundle.yaml must error instead of being silently dropped.
    #[test]
    fn bundle_yaml_unknown_key_errors() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("bad-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            // `features:` is valid; `native:` is a known typo of what used to be `vars:`.
            // `typo_key` is unknown and must produce an error.
            "typo_key:\n  value: oops\n",
        )
        .unwrap();

        let bundle = BundleRef {
            name: "bad-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let err = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap_err();
        assert!(
            err.to_string().contains("unknown key"),
            "must report unknown key, got: {err}"
        );
        assert!(
            err.to_string().contains("typo_key"),
            "error must name the unknown key, got: {err}"
        );
    }
}
