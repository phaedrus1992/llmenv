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

#[derive(Debug, Clone, Default)]
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
    /// The single active throttle entry after tag-intersection resolution, or
    /// `None` when no throttle entry is active for this scope.
    pub throttle: Option<crate::config::Throttle>,
    /// Resolved `session_log` config (see `Config::session_log_resolved`),
    /// gating which session-log hooks adapters auto-emit. Defaults to
    /// `SessionLog::default()` (transcript on, file off).
    pub session_log: crate::config::SessionLog,
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
        match std::fs::read_to_string(&am) {
            Ok(content) => agents_parts.push((b.name.clone(), content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        for sub in COPIED_SUBDIRS {
            let dir = b.path.join(sub);
            // #918: walk() tolerates a missing dir (NotFound) but propagates
            // other read errors, so an unreadable subdir surfaces instead of
            // an exists() stat masking it as absent.
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

    let mut merged_caps = merge_capabilities(&contributors)?;

    // #317: if slippage enabled with effort_level and no higher-precedence
    // effort_level was set, propagate from slippage config.
    if merged_caps.effort_level.is_none()
        && let Some(s) = merged_caps
            .features
            .as_ref()
            .and_then(|f| f.slippage.as_ref())
        && s.enabled
    {
        merged_caps.effort_level = s.effort_level.clone();
    }

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

/// Cheap, disk-cacheable signature of exactly the [`merge`] inputs that
/// determine `capabilities.features.memory` and `capabilities.host` (#920).
///
/// Reads only each bundle's `bundle.yaml` — not `AGENTS.md`, not the
/// `skills`/`plugins`/`hooks` subdirectories, not `rules/*.md` — so
/// recomputing it is far cheaper than a full [`merge`] call. Callers persist
/// the memory/host slice keyed on this signature and use a fresh computation
/// to check whether that persisted slice is still valid, without paying for
/// the full merge just to find out.
///
/// A signature match does not guarantee the *rest* of the manifest (files,
/// mcps, plugins, …) is unchanged — only that the memory/host-relevant inputs
/// are. Callers that need the full manifest must still call [`merge`].
///
/// `precedence` is hashed per bundle (not just `name`/`bundle.yaml` bytes):
/// `merge_capabilities` uses it to resolve `host`-key collisions across
/// bundles firing at different scope tiers, so a config edit that only
/// reassigns which scope kind fires a bundle (network/host/user/project —
/// see `cli::build_bundle_refs`) can flip the resolved `host` entry without
/// touching any `bundle.yaml` content. Without hashing precedence, that edit
/// would leave the signature unchanged and a stale cache entry would be
/// served as a hit — silently resolving to the wrong `host` address.
pub fn merge_signature(
    top_level: &Capabilities,
    native: &BTreeMap<String, serde_yaml::Value>,
    bundles: &[BundleRef],
) -> anyhow::Result<String> {
    use crate::materialize::cache::update_len_prefixed;
    use sha2::{Digest, Sha256};

    let mut h = Sha256::new();
    let top_yaml = serde_yaml::to_string(top_level)
        .map_err(|e| anyhow::anyhow!("serializing top-level capabilities: {e}"))?;
    update_len_prefixed(&mut h, top_yaml.as_bytes());

    h.update((native.len() as u64).to_le_bytes());
    for (key, value) in native {
        update_len_prefixed(&mut h, key.as_bytes());
        let serialized = serde_yaml::to_string(value)
            .map_err(|e| anyhow::anyhow!("serializing native key '{key}': {e}"))?;
        update_len_prefixed(&mut h, serialized.as_bytes());
    }

    // Sort by name so the signature doesn't depend on the order the caller's
    // ref-builder happened to enumerate bundles in (`cli::build_bundle_refs`
    // and `hook_run`'s caller may not agree on order) — `precedence` is
    // hashed per-bundle below, so this sort does not discard it.
    let mut sorted: Vec<&BundleRef> = bundles.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    h.update((sorted.len() as u64).to_le_bytes());
    for b in sorted {
        update_len_prefixed(&mut h, b.name.as_bytes());
        h.update([b.precedence]);
        let bytes = match std::fs::read(b.path.join("bundle.yaml")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        update_len_prefixed(&mut h, &bytes);
    }

    Ok(hex::encode(h.finalize()))
}

/// Keys that a `bundle.yaml` fragment is allowed to declare. Any other top-level
/// key is rejected with a hard error rather than silently dropped.
const BUNDLE_YAML_KNOWN_KEYS: &[&str] = &[
    "permissions",
    "hooks",
    "plugins",
    "mcp",
    "lsp",
    "skills",
    "env",
    "auto_memory_enabled",
    "effort_level",
    "advisor_size",
    "native_permissions",
    "native_hooks",
    "native_plugins",
    "native_mcp",
    "native_model_providers",
    "native",
    "features",
    "host",
];

/// Read an optional `bundle.yaml` capability fragment from a bundle directory.
/// Returns `None` when the file is absent — bundles carry capabilities only if
/// they choose to.
fn read_bundle_yaml(bundle_root: &Path, name: &str) -> anyhow::Result<Option<Capabilities>> {
    let path = bundle_root.join("bundle.yaml");
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "bundle '{name}': reading {}: {e}",
                path.display()
            ));
        }
    };
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
        crate::config::validate_capabilities_env_key(&context, key)?;
    }
    let permission_rules = caps
        .permissions
        .allow
        .iter()
        .chain(caps.permissions.ask.iter())
        .chain(caps.permissions.deny.iter());
    for rule in permission_rules {
        crate::config::validate_permission_rule(&context, rule)?;
    }
    for (engine, nr) in &caps.native_permissions {
        let ctx = format!("bundle '{name}': native_permissions['{engine}']");
        for s in nr.allow.iter().chain(nr.ask.iter()).chain(nr.deny.iter()) {
            crate::config::validate_permission_string(&ctx, s)?;
        }
    }

    // Validate bundle-contributed memory entries with the same checks that
    // Config::validate() applies to top-level features.memory entries.
    if let Some(features) = &caps.features {
        for mem in &features.memory {
            if mem.when.is_empty() {
                anyhow::bail!(
                    "{context}: features.memory entry for '{}' has no 'when' tags — every memory entry must declare at least one activation tag",
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
        for th in &features.throttle {
            if th.when.is_empty() {
                anyhow::bail!(
                    "{context}: features.throttle entry for '{}' has no 'when' tags — \
                     every throttle entry must declare at least one activation tag",
                    th.backend
                );
            }
            if th.backend.is_empty() {
                anyhow::bail!("{context}: features.throttle entry has an empty 'backend' field");
            }
        }
        for cm in &features.codebase_memory {
            if cm.when.is_empty() {
                anyhow::bail!(
                    "{context}: features.codebase_memory entry has no 'when' tags — \
                     every codebase_memory entry must declare at least one activation tag"
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
    // #918: NotFound (missing dir) → skip; other errors propagate.
    let Some(entries) = crate::paths::read_dir_optional(dir)? else {
        return Ok(());
    };
    for entry in entries {
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
                "      when: [home]\n",
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

    // #365: a bundle.yaml with a features.codebase_memory block contributes
    // entries to merged capabilities, mirroring the features.memory test above.
    #[test]
    fn bundle_features_codebase_memory_appears_in_merged_capabilities() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("cbm-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  codebase_memory:\n",
                "    - when: [my-project]\n",
            ),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "cbm-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();
        let features = manifest
            .capabilities
            .features
            .as_ref()
            .expect("features must be present");
        assert_eq!(features.codebase_memory.len(), 1);
        assert_eq!(features.codebase_memory[0].when, vec!["my-project"]);
    }

    // #365: a bundle-contributed codebase_memory entry with no `when` tags is
    // rejected at bundle-read time, mirroring the memory/throttle checks above.
    #[test]
    fn bundle_codebase_memory_without_tags_is_rejected() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("cbm-bad-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!("features:\n", "  codebase_memory:\n", "    - when: []\n",),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "cbm-bad-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let result = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("codebase_memory"),
            "error must mention codebase_memory"
        );
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

    // #373: reserved env key must produce an error matching ValidateError::CapabilitiesReservedEnvKey.
    #[test]
    fn bundle_env_reserved_key_is_rejected() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "env:\n  CLAUDE_CONFIG_DIR: /bad\n",
        )
        .unwrap();

        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };
        let err = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap_err();
        let ve = err
            .downcast_ref::<crate::config::ValidateError>()
            .expect("should be ValidateError");
        assert!(
            matches!(
                ve,
                crate::config::ValidateError::CapabilitiesReservedEnvKey { key, .. }
                    if key == "CLAUDE_CONFIG_DIR"
            ),
            "unexpected variant: {ve}"
        );
    }

    // #373: LLMENV_ prefix in bundle env must produce an error matching
    // ValidateError::CapabilitiesLlmenvPrefixEnvKey.
    #[test]
    fn bundle_env_llmenv_prefix_is_rejected() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "env:\n  LLMENV_CUSTOM: bad\n",
        )
        .unwrap();

        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };
        let err = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap_err();
        let ve = err
            .downcast_ref::<crate::config::ValidateError>()
            .expect("should be ValidateError");
        assert!(
            matches!(
                ve,
                crate::config::ValidateError::CapabilitiesLlmenvPrefixEnvKey { key, .. }
                    if key == "LLMENV_CUSTOM"
            ),
            "unexpected variant: {ve}"
        );
    }

    // #373: invalid var name in bundle env must produce an error matching
    // ValidateError::CapabilitiesInvalidVarName.
    #[test]
    fn bundle_env_invalid_var_name_is_rejected() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(bundle_dir.join("bundle.yaml"), "env:\n  1INVALID: bad\n").unwrap();

        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };
        let err = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap_err();
        let ve = err
            .downcast_ref::<crate::config::ValidateError>()
            .expect("should be ValidateError");
        assert!(
            matches!(
                ve,
                crate::config::ValidateError::CapabilitiesInvalidVarName { key, .. }
                    if key == "1INVALID"
            ),
            "unexpected variant: {ve}"
        );
    }

    // #664: a bundle-contributed deny pattern with an unmatched `(` (e.g. a
    // process-substitution pattern like `bash <(curl *`) must be rejected
    // rather than merged — Claude Code/Crush would otherwise silently drop
    // the whole rule at settings-load time, defeating the deny.
    #[test]
    fn bundle_permission_pattern_unbalanced_parens_is_rejected() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "permissions:\n",
                "  deny:\n",
                "    - tool: Bash\n",
                "      pattern: \"bash <(curl *\"\n",
            ),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };
        let err = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap_err();
        let ve = err
            .downcast_ref::<crate::config::ValidateError>()
            .expect("should be ValidateError");
        assert!(
            matches!(
                ve,
                crate::config::ValidateError::PermissionRuleUnbalancedParens { tool, .. }
                    if tool == "Bash"
            ),
            "unexpected variant: {ve}"
        );
    }

    // Balanced parens in a bundle-contributed pattern must merge cleanly.
    #[test]
    fn bundle_permission_pattern_balanced_parens_is_accepted() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "permissions:\n",
                "  deny:\n",
                "    - tool: Bash\n",
                "      pattern: \"bash <(curl *)*\"\n",
            ),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };
        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();
        assert_eq!(manifest.capabilities.permissions.deny.len(), 1);
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

    // #317: post-merge — slippage.effort_level propagates when no higher-prec
    // effort_level is set directly.
    #[test]
    fn post_merge_effort_level_from_slippage() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("slippage-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  slippage:\n",
                "    enabled: true\n",
                "    effort_level: xhigh\n",
            ),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "slippage-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let manifest = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();
        assert_eq!(
            manifest.capabilities.effort_level.as_deref(),
            Some("xhigh"),
            "slippage.effort_level must propagate to capabilities.effort_level"
        );
    }

    // #317: post-merge — config.yaml-effort_level wins over slippage.effort_level.
    #[test]
    fn post_merge_effort_level_not_override() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("slippage-bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  slippage:\n",
                "    enabled: true\n",
                "    effort_level: xhigh\n",
            ),
        )
        .unwrap();

        let bundle = BundleRef {
            name: "slippage-bundle".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let top_caps = Capabilities {
            effort_level: Some("low".into()),
            ..Default::default()
        };

        let manifest = merge(&top_caps, &BTreeMap::new(), &[bundle]).unwrap();
        assert_eq!(
            manifest.capabilities.effort_level.as_deref(),
            Some("low"),
            "direct effort_level must win over slippage-derived effort_level"
        );
    }

    // #920: same inputs must produce the same signature — required for the
    // persisted merge-cache lookup to ever hit.
    #[test]
    fn merge_signature_stable_for_identical_inputs() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "features:\n  memory:\n    - server_host: h\n      port: 1\n",
        )
        .unwrap();
        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let sig1 = merge_signature(
            &Capabilities::default(),
            &BTreeMap::new(),
            std::slice::from_ref(&bundle),
        )
        .unwrap();
        let sig2 = merge_signature(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();
        assert_eq!(sig1, sig2);
    }

    // #920: a persisted cache spanning multiple `regenerate` runs must catch
    // bundle *content* edits, not just config.yaml edits (unlike the
    // in-process `merge_cache_key` in hook_run, which only needs mtime+names
    // because it never survives a bundle edit within a single session).
    #[test]
    fn merge_signature_changes_when_bundle_yaml_content_changes() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "features:\n  memory:\n    - server_host: h\n      port: 1\n",
        )
        .unwrap();
        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir.clone(),
            precedence: 1,
        };
        let before = merge_signature(
            &Capabilities::default(),
            &BTreeMap::new(),
            std::slice::from_ref(&bundle),
        )
        .unwrap();

        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            "features:\n  memory:\n    - server_host: h\n      port: 2\n",
        )
        .unwrap();
        let after = merge_signature(&Capabilities::default(), &BTreeMap::new(), &[bundle]).unwrap();

        assert_ne!(
            before, after,
            "editing bundle.yaml content must invalidate the signature"
        );
    }

    // #920: a different firing bundle set must produce a different signature —
    // otherwise a hook-run in a project scope (extra project-tagged bundles
    // firing) could reuse a signature computed for a narrower scope.
    #[test]
    fn merge_signature_changes_when_firing_bundle_set_changes() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(bundle_dir.join("bundle.yaml"), "features:\n  memory: []\n").unwrap();
        let bundle = BundleRef {
            name: "b".into(),
            path: bundle_dir,
            precedence: 1,
        };
        let other_dir = tmp.path().join("c");
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(other_dir.join("bundle.yaml"), "features:\n  memory: []\n").unwrap();
        let other = BundleRef {
            name: "c".into(),
            path: other_dir,
            precedence: 1,
        };

        let one = merge_signature(
            &Capabilities::default(),
            &BTreeMap::new(),
            std::slice::from_ref(&bundle),
        )
        .unwrap();
        let two =
            merge_signature(&Capabilities::default(), &BTreeMap::new(), &[bundle, other]).unwrap();

        assert_ne!(
            one, two,
            "adding a firing bundle must invalidate the signature"
        );
    }

    // #920: top-level `capabilities.host`/`features.memory` changes must
    // invalidate the signature even with no bundles at all.
    #[test]
    fn merge_signature_changes_when_top_level_capabilities_change() {
        let mut host_a = BTreeMap::new();
        host_a.insert(
            "srv".to_string(),
            crate::config::HostEntry {
                addr: "1.1.1.1".into(),
            },
        );
        let caps_a = Capabilities {
            host: host_a,
            ..Default::default()
        };
        let mut host_b = BTreeMap::new();
        host_b.insert(
            "srv".to_string(),
            crate::config::HostEntry {
                addr: "2.2.2.2".into(),
            },
        );
        let caps_b = Capabilities {
            host: host_b,
            ..Default::default()
        };

        let sig_a = merge_signature(&caps_a, &BTreeMap::new(), &[]).unwrap();
        let sig_b = merge_signature(&caps_b, &BTreeMap::new(), &[]).unwrap();
        assert_ne!(sig_a, sig_b);
    }

    // #920: the `native:` map feeds into the merge too — a change there must
    // also invalidate the signature.
    #[test]
    fn merge_signature_changes_when_native_map_changes() {
        let mut native_a: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        native_a.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: a").unwrap(),
        );
        let mut native_b: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        native_b.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: b").unwrap(),
        );

        let sig_a = merge_signature(&Capabilities::default(), &native_a, &[]).unwrap();
        let sig_b = merge_signature(&Capabilities::default(), &native_b, &[]).unwrap();
        assert_ne!(sig_a, sig_b);
    }

    // #920: a missing bundle.yaml is a legitimate state (bundles carry
    // capabilities only if they choose to, per `read_bundle_yaml`) — the
    // signature must not error, matching `merge`'s own tolerance.
    #[test]
    fn merge_signature_tolerates_missing_bundle_yaml() {
        let tmp = tempdir().unwrap();
        let bundle_dir = tmp.path().join("no-yaml");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let bundle = BundleRef {
            name: "no-yaml".into(),
            path: bundle_dir,
            precedence: 1,
        };

        let sig = merge_signature(&Capabilities::default(), &BTreeMap::new(), &[bundle]);
        assert!(sig.is_ok());
    }

    mod merge_signature_proptests {
        use super::*;
        use proptest::prelude::*;

        // #920: identified during pre-pr-review as the highest-value property
        // to check — `merge_signature` is a cache key, so non-determinism
        // across arbitrary inputs (not just the hand-picked example cases
        // above) would silently make the persisted cache never reliably hit.
        proptest! {
            #[test]
            fn signature_is_deterministic(
                body in ".{0,64}",
                precedence in any::<u8>(),
                native_key in "[a-z]{1,8}",
                native_val in "[a-z]{0,16}",
            ) {
                let tmp = tempdir().unwrap();
                let bundle_dir = tmp.path().join("b");
                std::fs::create_dir_all(&bundle_dir).unwrap();
                std::fs::write(bundle_dir.join("bundle.yaml"), &body).unwrap();
                let bundle = BundleRef {
                    name: "b".into(),
                    path: bundle_dir,
                    precedence,
                };
                let mut native: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
                native.insert(native_key, serde_yaml::Value::String(native_val));

                let sig1 = merge_signature(&Capabilities::default(), &native, std::slice::from_ref(&bundle));
                let sig2 = merge_signature(&Capabilities::default(), &native, std::slice::from_ref(&bundle));
                prop_assert_eq!(sig1.unwrap(), sig2.unwrap());
            }

            #[test]
            fn signature_is_order_independent_over_bundle_set(
                name_a in "[a-z]{1,8}", name_b in "[a-z]{1,8}",
            ) {
                prop_assume!(name_a != name_b);
                let tmp = tempdir().unwrap();
                let mk = |name: &str| {
                    let dir = tmp.path().join(name);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("bundle.yaml"), "features:\n  memory: []\n").unwrap();
                    BundleRef { name: name.into(), path: dir, precedence: 1 }
                };
                let a = mk(&name_a);
                let b = mk(&name_b);

                let forward = merge_signature(&Capabilities::default(), &BTreeMap::new(), &[a.clone(), b.clone()]);
                let backward = merge_signature(&Capabilities::default(), &BTreeMap::new(), &[b, a]);
                prop_assert_eq!(forward.unwrap(), backward.unwrap());
            }
        }
    }
}
