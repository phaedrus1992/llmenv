//! Validation of `native_*.<engine>` map keys against the registered adapters.
//!
//! Every per-engine capability map is keyed by an arbitrary string that serde
//! accepts verbatim. Adapters then look up only their own id (`.get("opencode")`),
//! so a typo — or a key naming an engine whose adapter never reads that map —
//! deserializes, merges, and hashes cleanly and is then dropped on the floor
//! with no diagnostic (#1032).

use std::collections::BTreeMap;

use crate::adapter::{engine_id, registered_adapters};
use crate::config::Capabilities;

/// Config field names of the per-engine `native_*` maps. Adapters name the ones
/// they read via [`AgentAdapter::native_maps`]; sharing the constants keeps a
/// declaration from drifting from the field by a typo.
pub(crate) const NATIVE_PERMISSIONS: &str = "native_permissions";
pub(crate) const NATIVE_HOOKS: &str = "native_hooks";
pub(crate) const NATIVE_PLUGINS: &str = "native_plugins";
pub(crate) const NATIVE_MCP: &str = "native_mcp";
pub(crate) const NATIVE_MODEL_PROVIDERS: &str = "native_model_providers";
/// The catch-all `native:` block. Top-level `config.native` and the
/// bundle-contributed `capabilities.native` both merge into the same rendered
/// place, so adapters declare this one name for both.
pub(crate) const NATIVE: &str = "native";

/// Why a `native_*.<engine>` key never reaches an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeadKeyReason {
    /// No registered adapter uses this engine id — almost always a typo.
    /// Adapters key off the exact string, so case differences count as typos.
    UnknownEngine,
    /// The engine is registered but its adapter never reads this map.
    MapNotRead,
}

/// A `native_*` key that no adapter will ever consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeadNativeKey {
    /// Config field the key lives under, e.g. `native_mcp`. Carries a
    /// `top-level `/`capabilities.` qualifier for the two `native:` blocks.
    pub label: String,
    /// The `native_*` map this key belongs to, as named by
    /// [`AgentAdapter::native_maps`].
    pub map: &'static str,
    /// The offending key exactly as written in the config.
    pub key: String,
    pub reason: DeadKeyReason,
}

impl DeadNativeKey {
    /// One-line diagnostic naming the key, why it does nothing, and where the
    /// setting actually belongs.
    pub fn message(&self) -> String {
        let Self {
            label,
            map,
            key,
            reason,
        } = self;
        match reason {
            DeadKeyReason::UnknownEngine => format!(
                "{label} key '{key}' is not a registered engine (known: {}) — the block is \
                 accepted by the config schema but never rendered",
                crate::adapter::known_engine_ids().join(", ")
            ),
            DeadKeyReason::MapNotRead => format!(
                "{label} key '{key}' names a registered engine whose adapter never reads \
                 {map} — the block is accepted by the config schema but never rendered. {}",
                neutral_redirect(map)
            ),
        }
    }
}

/// Where a setting belongs when the engine's adapter doesn't read the
/// per-engine map. Mirrors the wording of `modeled_key_redirect` in
/// `super`, which answers the same question from the opposite direction.
fn neutral_redirect(map: &str) -> String {
    let neutral = match map {
        NATIVE_PERMISSIONS => "permissions",
        NATIVE_HOOKS => "hooks",
        NATIVE_PLUGINS => "plugins",
        NATIVE_MCP => "mcp",
        NATIVE_MODEL_PROVIDERS => "model_providers",
        // The catch-all block has no neutral counterpart to redirect to.
        _ => {
            return "This engine has no such passthrough — drop the block or move it under an \
                    engine that does."
                .to_string();
        }
    };
    format!("Declare it through `capabilities.{neutral}` instead.")
}

/// Returns every `native_*` key that no adapter will read, across all six
/// per-engine maps plus both `native:` blocks.
///
/// Takes the **merged** capabilities and top-level `native` block, not the raw
/// top-level config: `bundle.yaml` may contribute to every one of these maps
/// (see `BUNDLE_YAML_KNOWN_KEYS`), and a typo in a shared bundle is the case
/// most worth catching. Pass `manifest.capabilities` / `manifest.native` from a
/// built [`crate::merge::MergedManifest`], or the raw `Config` fields when
/// validating the config file itself.
///
/// Ordering is stable — maps in declaration order, keys in `BTreeMap` order —
/// so callers can print without sorting and the output is diffable.
pub(crate) fn dead_native_engine_keys(
    capabilities: &Capabilities,
    top_level_native: &BTreeMap<String, serde_yaml::Value>,
) -> Vec<DeadNativeKey> {
    let adapters = registered_adapters();
    let caps = capabilities;

    // (diagnostic label, map name, keys). The label distinguishes the two
    // `native:` blocks, which share one map name because they render together.
    let maps: [(&str, &'static str, Vec<&String>); 7] = [
        (
            NATIVE_PERMISSIONS,
            NATIVE_PERMISSIONS,
            caps.native_permissions.keys().collect(),
        ),
        (
            NATIVE_HOOKS,
            NATIVE_HOOKS,
            caps.native_hooks.keys().collect(),
        ),
        (
            NATIVE_PLUGINS,
            NATIVE_PLUGINS,
            caps.native_plugins.keys().collect(),
        ),
        (NATIVE_MCP, NATIVE_MCP, caps.native_mcp.keys().collect()),
        (
            NATIVE_MODEL_PROVIDERS,
            NATIVE_MODEL_PROVIDERS,
            caps.native_model_providers.keys().collect(),
        ),
        (
            "top-level native",
            NATIVE,
            top_level_native.keys().collect(),
        ),
        ("capabilities.native", NATIVE, caps.native.keys().collect()),
    ];

    let mut dead = Vec::new();
    for (label, map, keys) in maps {
        for key in keys {
            let adapter = adapters
                .iter()
                .find(|a| engine_id(a.as_ref()) == *key)
                .map(std::convert::AsRef::as_ref);
            let reason = match adapter {
                None => DeadKeyReason::UnknownEngine,
                Some(a) if !a.native_maps().contains(&map) => DeadKeyReason::MapNotRead,
                Some(_) => continue,
            };
            dead.push(DeadNativeKey {
                label: label.to_string(),
                map,
                key: key.clone(),
                reason,
            });
        }
    }
    dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpServer, McpTransport, NativePermissionRules};

    /// Single-key capability map holding an empty native block.
    fn yaml_keyed(key: &str) -> BTreeMap<String, serde_yaml::Value> {
        BTreeMap::from([(
            key.into(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        )])
    }

    fn perms_keyed(key: &str) -> BTreeMap<String, NativePermissionRules> {
        BTreeMap::from([(key.into(), NativePermissionRules::default())])
    }

    fn dead(caps: &Capabilities) -> Vec<DeadNativeKey> {
        dead_native_engine_keys(caps, &BTreeMap::new())
    }

    fn mcp_named(name: &str) -> McpServer {
        McpServer {
            name: name.into(),
            when: vec![],
            transport: McpTransport::Stdio,
            command: Some("echo".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            disabled: false,
            disabled_tools: vec![],
            timeout: None,
        }
    }

    #[test]
    fn empty_config_has_no_dead_keys() {
        assert!(dead(&Capabilities::default()).is_empty());
    }

    /// One engine per map that genuinely reads it — see the consumption matrix
    /// asserted by `native_maps_match_actual_consumers` in `super`.
    #[test]
    fn every_map_accepts_an_engine_that_reads_it() {
        let caps = Capabilities {
            native_permissions: perms_keyed("opencode"),
            native_hooks: yaml_keyed("crush"),
            native_plugins: yaml_keyed("claude_code"),
            native_mcp: yaml_keyed("opencode"),
            native_model_providers: yaml_keyed("crush"),
            native: yaml_keyed("claude_code"),
            ..Capabilities::default()
        };
        let found = dead_native_engine_keys(&caps, &yaml_keyed("opencode"));
        assert!(found.is_empty(), "expected empty: {found:?}");
    }

    #[test]
    fn typo_flagged_in_every_map() {
        let caps = Capabilities {
            native_permissions: perms_keyed("opencde"),
            native_hooks: yaml_keyed("opencde"),
            native_plugins: yaml_keyed("opencde"),
            native_mcp: yaml_keyed("opencde"),
            native_model_providers: yaml_keyed("opencde"),
            native: yaml_keyed("opencde"),
            ..Capabilities::default()
        };
        let found = dead_native_engine_keys(&caps, &yaml_keyed("opencde"));
        assert_eq!(found.len(), 7, "one per map: {found:?}");
        assert!(
            found
                .iter()
                .all(|d| d.reason == DeadKeyReason::UnknownEngine && d.key == "opencde")
        );
        let labels: Vec<&str> = found.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "native_permissions",
                "native_hooks",
                "native_plugins",
                "native_mcp",
                "native_model_providers",
                "top-level native",
                "capabilities.native",
            ]
        );
    }

    /// Claude Code is Anthropic-only and reads no provider block (#1032 case 2).
    #[test]
    fn model_providers_flagged_for_engine_that_does_not_read_it() {
        let caps = Capabilities {
            native_model_providers: yaml_keyed("claude_code"),
            ..Capabilities::default()
        };
        assert_eq!(
            dead(&caps),
            vec![DeadNativeKey {
                label: NATIVE_MODEL_PROVIDERS.to_string(),
                map: NATIVE_MODEL_PROVIDERS,
                key: "claude_code".into(),
                reason: DeadKeyReason::MapNotRead,
            }]
        );
    }

    /// Crush renders plugins from nothing — only Claude Code reads the map.
    #[test]
    fn plugins_flagged_for_crush() {
        let caps = Capabilities {
            native_plugins: yaml_keyed("crush"),
            ..Capabilities::default()
        };
        let found = dead(&caps);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].reason, DeadKeyReason::MapNotRead);
    }

    /// The regression this replaced a capability-predicate gate to catch:
    /// opencode reports `supports_plugins() == true` and a non-empty
    /// `supported_hook_events()`, but reads neither map.
    #[test]
    fn opencode_hooks_and_plugins_are_flagged_despite_capability_predicates() {
        let caps = Capabilities {
            native_hooks: yaml_keyed("opencode"),
            native_plugins: yaml_keyed("opencode"),
            ..Capabilities::default()
        };
        let found = dead(&caps);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|d| d.reason == DeadKeyReason::MapNotRead));
        assert!(found.iter().all(|d| d.key == "opencode"));
    }

    /// `native_permissions` is keyed by engine, not by MCP server name — every
    /// consumer is an exact engine lookup, so a server name there is dead
    /// config and must be reported rather than exempted.
    #[test]
    fn native_permissions_mcp_server_name_is_dead() {
        let caps = Capabilities {
            native_permissions: perms_keyed("my-server"),
            ..Capabilities::default()
        };
        let found = dead(&caps);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].reason, DeadKeyReason::UnknownEngine);
        // Configuring the server changes nothing — the map is engine-keyed.
        assert_eq!(dead(&caps).len(), 1, "{:?}", mcp_named("my-server").name);
    }

    #[test]
    fn native_permissions_flags_unconfigured_mcp_name() {
        let caps = Capabilities {
            native_permissions: perms_keyed("mcp__unknown-server"),
            ..Capabilities::default()
        };
        let found = dead(&caps);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "mcp__unknown-server");
        assert_eq!(found[0].reason, DeadKeyReason::UnknownEngine);
    }

    /// Adapters look keys up with an exact `.get()`, so a case variant really is
    /// dead config and must be reported rather than accepted.
    #[test]
    fn case_variant_of_engine_id_is_dead() {
        let found = dead_native_engine_keys(&Capabilities::default(), &yaml_keyed("Claude_Code"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reason, DeadKeyReason::UnknownEngine);
    }

    #[test]
    fn unknown_engine_message_lists_known_ids() {
        let msg = DeadNativeKey {
            label: NATIVE_MCP.to_string(),
            map: NATIVE_MCP,
            key: "opencde".into(),
            reason: DeadKeyReason::UnknownEngine,
        }
        .message();
        assert!(msg.contains("native_mcp key 'opencde'"), "{msg}");
        for id in crate::adapter::known_engine_ids() {
            assert!(msg.contains(&id), "message should list {id}: {msg}");
        }
    }

    #[test]
    fn map_not_read_message_names_the_neutral_field() {
        let msg = DeadNativeKey {
            label: NATIVE_MODEL_PROVIDERS.to_string(),
            map: NATIVE_MODEL_PROVIDERS,
            key: "claude_code".into(),
            reason: DeadKeyReason::MapNotRead,
        }
        .message();
        assert!(msg.contains("claude_code"), "{msg}");
        assert!(msg.contains("capabilities.model_providers"), "{msg}");
    }

    /// The catch-all block has no neutral counterpart, so it must not claim one.
    #[test]
    fn map_not_read_message_for_native_block_has_no_neutral_field() {
        let msg = DeadNativeKey {
            label: "top-level native".to_string(),
            map: NATIVE,
            key: "claude_code".into(),
            reason: DeadKeyReason::MapNotRead,
        }
        .message();
        assert!(!msg.contains("capabilities."), "{msg}");
    }
}
