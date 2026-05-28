#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use llmenv::config::{Capabilities, Config};
use llmenv::merge::merge;

/// Issue #96: Config deserializes top-level native.claude_code passthrough
#[test]
fn config_deserializes_native_passthrough() {
    let yaml = r#"
native:
  claude_code:
    alwaysThinkingEnabled: true
    outputStyle: "verbose"
"#;

    let config: Config = serde_yaml::from_str(yaml).expect("parse config with native");
    assert!(!config.native.is_empty(), "native map should not be empty");
    assert!(
        config.native.contains_key("claude_code"),
        "should have claude_code key"
    );
}

/// Issue #96: native field preserves engine-specific values
#[test]
fn native_preserves_engine_values() {
    let yaml = r#"
native:
  claude_code:
    customFlag: true
    customString: "value"
  other_engine:
    someKey: 123
"#;

    let config: Config = serde_yaml::from_str(yaml).expect("parse");
    assert_eq!(config.native.len(), 2, "should have 2 engines");
    assert!(config.native.contains_key("claude_code"));
    assert!(config.native.contains_key("other_engine"));
}

/// Issue #96: MergedManifest has a native field and carries values through merge
#[test]
fn merged_manifest_carries_native() {
    use std::collections::BTreeMap;

    // Create native values
    let mut native = BTreeMap::new();
    let mut claude_code_native = serde_yaml::Mapping::new();
    claude_code_native.insert(
        serde_yaml::Value::String("alwaysThinkingEnabled".to_string()),
        serde_yaml::Value::Bool(true),
    );
    native.insert(
        "claude_code".to_string(),
        serde_yaml::Value::Mapping(claude_code_native),
    );

    // Merge with empty bundles - should preserve native in MergedManifest
    let merged = merge(&Capabilities::default(), &native, &[]).expect("merge");

    // Should have native field with our values
    assert!(!merged.native.is_empty());
    assert!(merged.native.contains_key("claude_code"));
}

/// Issue #97: Capabilities carries a container-level `native_hooks` map (one
/// per-engine fragment list, sibling to the generic `hooks` list — mirroring
/// `permissions.native`). The engine-only hook registrations live verbatim under
/// their engine key.
#[test]
fn capabilities_hooks_have_native_overrides() {
    let yaml = r#"
hooks:
  - event: PreToolUse
    handler:
      type: command
      command: "hooks/check.sh"
native_hooks:
  claude_code:
    - event: PreCompact
      hooks:
        - type: command
          command: "/bin/engine-only.sh"
"#;
    let caps: Capabilities = serde_yaml::from_str(yaml).expect("parse hooks with native");
    assert_eq!(caps.hooks.len(), 1, "generic hook preserved");
    assert!(
        caps.native_hooks.contains_key("claude_code"),
        "native_hooks should have claude_code key"
    );
}

/// Issue #97: Capabilities carries a container-level `native_plugins` map
/// (per-engine opaque fragments, sibling to the generic `plugins` list).
#[test]
fn capabilities_plugins_have_native_overrides() {
    let yaml = r#"
plugins:
  - "superpowers:brainstorming"
native_plugins:
  claude_code:
    plugin_flag: true
"#;
    let caps: Capabilities = serde_yaml::from_str(yaml).expect("parse plugins with native");
    assert_eq!(caps.plugins.len(), 1, "generic plugin preserved");
    assert!(
        caps.native_plugins.contains_key("claude_code"),
        "native_plugins should have claude_code key"
    );
}

/// Issue #97: Capabilities carries a container-level `native_mcp` map (per-engine
/// opaque fragments, sibling to the resolved MCP list).
#[test]
fn capabilities_mcp_have_native_overrides() {
    let yaml = r#"
native_mcp:
  claude_code:
    enabledMcpjsonServers:
      - stdio_server
"#;
    let caps: Capabilities = serde_yaml::from_str(yaml).expect("parse mcp with native");
    assert!(
        caps.native_mcp.contains_key("claude_code"),
        "native_mcp should have claude_code key"
    );
}
