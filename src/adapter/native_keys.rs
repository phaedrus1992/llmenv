//! Validation of `native_*.<engine>` map keys against the registered adapters.
//!
//! Every per-engine capability map is keyed by an arbitrary string that serde
//! accepts verbatim. Adapters then look up only their own id (`.get("opencode")`),
//! so a typo or a feature/engine mismatch deserializes, merges, and hashes
//! cleanly and is then dropped on the floor with no diagnostic (#1032).

use crate::adapter::{AgentAdapter, engine_id, registered_adapters};
use crate::config::Config;

/// Why a `native_*.<engine>` key will never reach an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeadKeyReason {
    /// No registered adapter uses this engine id — almost always a typo.
    /// Adapters key off the exact string, so case differences count as typos.
    UnknownEngine,
    /// The engine is registered but has no such feature, so it never reads the
    /// block. `feature` is the human-readable feature name for the diagnostic.
    FeatureUnsupported { feature: &'static str },
}

/// A `native_*` key that no adapter will ever consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeadNativeKey {
    /// Config field the key lives under, e.g. `native_mcp`.
    pub map: &'static str,
    /// The offending key exactly as written in the config.
    pub key: String,
    pub reason: DeadKeyReason,
}

impl DeadNativeKey {
    /// One-line diagnostic naming the map, the key, and why it does nothing.
    pub fn message(&self) -> String {
        let Self { map, key, reason } = self;
        match reason {
            DeadKeyReason::UnknownEngine => format!(
                "{map} key '{key}' is not a registered engine (known: {}) — the block is \
                 accepted by the config schema but never rendered",
                known_engine_ids_csv()
            ),
            DeadKeyReason::FeatureUnsupported { feature } => format!(
                "{map} key '{key}' targets a registered engine that has no {feature} support — \
                 the block is accepted by the config schema but never rendered"
            ),
        }
    }
}

fn known_engine_ids_csv() -> String {
    crate::adapter::known_engine_ids().join(", ")
}

/// Whether an engine consumes a given `native_*` map at all. `None` means every
/// engine reads it, so only the engine id itself needs checking.
type FeatureGate = Option<(&'static str, fn(&dyn AgentAdapter) -> bool)>;

fn hooks_gate(adapter: &dyn AgentAdapter) -> bool {
    !adapter.supported_hook_events().is_empty()
}

fn plugins_gate(adapter: &dyn AgentAdapter) -> bool {
    adapter.supports_plugins()
}

fn model_providers_gate(adapter: &dyn AgentAdapter) -> bool {
    adapter.supports_model_providers()
}

/// Returns every `native_*` key across all six maps that no adapter will read.
///
/// Ordering is stable: maps in declaration order, keys in `BTreeMap` order
/// within each map, so callers can print without sorting.
pub(crate) fn dead_native_engine_keys(config: &Config) -> Vec<DeadNativeKey> {
    let adapters = registered_adapters();
    let engine_of = |key: &str| {
        adapters
            .iter()
            .find(|a| engine_id(a.as_ref()) == key)
            .map(std::convert::AsRef::as_ref)
    };

    let caps = &config.capabilities;
    let permission_key_aliases = permission_key_aliases(config);

    // (field name, keys, feature gate, extra keys that are valid for reasons
    // other than being an engine id).
    let maps: [(&'static str, Vec<&String>, FeatureGate, bool); 7] = [
        (
            "native_permissions",
            caps.native_permissions.keys().collect(),
            None,
            true,
        ),
        (
            "native_hooks",
            caps.native_hooks.keys().collect(),
            Some(("hook", hooks_gate)),
            false,
        ),
        (
            "native_plugins",
            caps.native_plugins.keys().collect(),
            Some(("plugin", plugins_gate)),
            false,
        ),
        ("native_mcp", caps.native_mcp.keys().collect(), None, false),
        (
            "native_model_providers",
            caps.native_model_providers.keys().collect(),
            Some(("model-provider", model_providers_gate)),
            false,
        ),
        ("native", config.native.keys().collect(), None, false),
        (
            "capabilities.native",
            caps.native.keys().collect(),
            None,
            false,
        ),
    ];

    let mut dead = Vec::new();
    for (map, keys, gate, allow_mcp_aliases) in maps {
        for key in keys {
            if allow_mcp_aliases && permission_key_aliases.contains(key.as_str()) {
                continue;
            }
            let Some(adapter) = engine_of(key) else {
                dead.push(DeadNativeKey {
                    map,
                    key: key.clone(),
                    reason: DeadKeyReason::UnknownEngine,
                });
                continue;
            };
            if let Some((feature, supported)) = gate
                && !supported(adapter)
            {
                dead.push(DeadNativeKey {
                    map,
                    key: key.clone(),
                    reason: DeadKeyReason::FeatureUnsupported { feature },
                });
            }
        }
    }
    dead
}

/// Keys that are legitimately valid in `native_permissions` without naming an
/// engine: `native_permissions` doubles as the per-MCP-server permission map,
/// so a configured server name (plus llmenv's own always-present `icm`) is a
/// real key, not a typo.
fn permission_key_aliases(config: &Config) -> std::collections::HashSet<&str> {
    config
        .mcp
        .iter()
        .map(|m| m.name.as_str())
        .chain(std::iter::once("icm"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Capabilities, McpServer, McpTransport, NativePermissionRules};
    use std::collections::BTreeMap;

    fn yaml_map() -> serde_yaml::Value {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
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
        assert!(dead_native_engine_keys(&Config::default()).is_empty());
    }

    #[test]
    fn every_map_accepts_a_registered_engine() {
        let config = Config {
            native: BTreeMap::from([("crush".into(), yaml_map())]),
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "opencode".into(),
                    NativePermissionRules::default(),
                )]),
                native_hooks: BTreeMap::from([("claude_code".into(), yaml_map())]),
                native_plugins: BTreeMap::from([("opencode".into(), yaml_map())]),
                native_mcp: BTreeMap::from([("crush".into(), yaml_map())]),
                native_model_providers: BTreeMap::from([("opencode".into(), yaml_map())]),
                native: BTreeMap::from([("claude_code".into(), yaml_map())]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert!(dead.is_empty(), "expected empty: {dead:?}");
    }

    #[test]
    fn typo_flagged_in_every_map() {
        let config = Config {
            native: BTreeMap::from([("opencde".into(), yaml_map())]),
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "opencde".into(),
                    NativePermissionRules::default(),
                )]),
                native_hooks: BTreeMap::from([("opencde".into(), yaml_map())]),
                native_plugins: BTreeMap::from([("opencde".into(), yaml_map())]),
                native_mcp: BTreeMap::from([("opencde".into(), yaml_map())]),
                native_model_providers: BTreeMap::from([("opencde".into(), yaml_map())]),
                native: BTreeMap::from([("opencde".into(), yaml_map())]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert_eq!(dead.len(), 7, "one per map: {dead:?}");
        assert!(
            dead.iter()
                .all(|d| d.reason == DeadKeyReason::UnknownEngine && d.key == "opencde")
        );
        let maps: Vec<&str> = dead.iter().map(|d| d.map).collect();
        assert_eq!(
            maps,
            vec![
                "native_permissions",
                "native_hooks",
                "native_plugins",
                "native_mcp",
                "native_model_providers",
                "native",
                "capabilities.native",
            ]
        );
    }

    #[test]
    fn model_providers_flagged_for_engine_without_provider_support() {
        let config = Config {
            capabilities: Capabilities {
                native_model_providers: BTreeMap::from([("claude_code".into(), yaml_map())]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert_eq!(
            dead,
            vec![DeadNativeKey {
                map: "native_model_providers",
                key: "claude_code".into(),
                reason: DeadKeyReason::FeatureUnsupported {
                    feature: "model-provider"
                },
            }]
        );
    }

    #[test]
    fn plugins_flagged_for_engine_without_plugin_support() {
        let config = Config {
            capabilities: Capabilities {
                native_plugins: BTreeMap::from([("crush".into(), yaml_map())]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert_eq!(
            dead,
            vec![DeadNativeKey {
                map: "native_plugins",
                key: "crush".into(),
                reason: DeadKeyReason::FeatureUnsupported { feature: "plugin" },
            }]
        );
    }

    #[test]
    fn native_permissions_accepts_configured_mcp_and_icm() {
        let config = Config {
            mcp: vec![mcp_named("my-server")],
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([
                    ("my-server".into(), NativePermissionRules::default()),
                    ("icm".into(), NativePermissionRules::default()),
                ]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert!(dead.is_empty(), "expected empty: {dead:?}");
    }

    #[test]
    fn native_permissions_flags_unconfigured_mcp_name() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "mcp__unknown-server".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].key, "mcp__unknown-server");
        assert_eq!(dead[0].reason, DeadKeyReason::UnknownEngine);
    }

    /// The MCP-name alias only applies to `native_permissions`; every other map
    /// is engine-keyed, so an MCP server name there is dead config.
    #[test]
    fn mcp_alias_does_not_leak_into_other_maps() {
        let config = Config {
            mcp: vec![mcp_named("my-server")],
            capabilities: Capabilities {
                native_mcp: BTreeMap::from([("my-server".into(), yaml_map())]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert_eq!(dead[0].map, "native_mcp");
    }

    /// Adapters look keys up with an exact `.get()`, so a case variant really is
    /// dead config and must be reported rather than accepted.
    #[test]
    fn case_variant_of_engine_id_is_dead() {
        let config = Config {
            native: BTreeMap::from([("Claude_Code".into(), yaml_map())]),
            ..Config::default()
        };
        let dead = dead_native_engine_keys(&config);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].reason, DeadKeyReason::UnknownEngine);
    }

    #[test]
    fn unknown_engine_message_lists_known_ids() {
        let msg = DeadNativeKey {
            map: "native_mcp",
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
    fn feature_unsupported_message_names_the_feature() {
        let msg = DeadNativeKey {
            map: "native_model_providers",
            key: "claude_code".into(),
            reason: DeadKeyReason::FeatureUnsupported {
                feature: "model-provider",
            },
        }
        .message();
        assert!(msg.contains("model-provider"), "{msg}");
        assert!(msg.contains("claude_code"), "{msg}");
    }
}
