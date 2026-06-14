#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
use llmenv::config::{Capabilities, Config};
use llmenv::merge::{BundleRef, merge};

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

/// Issue #291: a bundle.yaml with a native: block must appear in MergedManifest.native.
/// Repro: bundle contributes native.claude_code.statusLine — it was silently dropped before this fix.
#[test]
fn bundle_native_block_is_rendered_in_merged_output() {
    use std::collections::BTreeMap;

    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = tmp.path().join("my-bundle");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        "native:\n  claude_code:\n    statusLine: foo\n",
    )
    .unwrap();

    let bundle = BundleRef {
        name: "my-bundle".into(),
        path: bundle_dir,
        precedence: 1,
    };

    let merged = merge(&Capabilities::default(), &BTreeMap::new(), &[bundle]).expect("merge");

    assert!(
        merged.native.contains_key("claude_code"),
        "bundle native: block must appear in MergedManifest.native (repro #291)"
    );
    let status = merged.native["claude_code"]
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("statusLine".into())))
        .and_then(serde_yaml::Value::as_str)
        .expect("statusLine must be present");
    assert_eq!(status, "foo", "statusLine value must be preserved");
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
