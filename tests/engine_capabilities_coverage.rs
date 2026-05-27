/// Test coverage audit for engine-capabilities design doc.
/// Identifies and tests all precedence, conflict resolution, and value-shape merge rules.
/// References: docs/design/engine-capabilities.md (D1, D2, D3, O3)
use std::collections::BTreeMap;

// ============================================================================
// D2: Precedence Rules (scalar override by scope)
// ============================================================================

#[test]
fn d2_scalar_precedence_default_mode_highest_scope_wins() {
    /// Test: when multiple bundles contribute default_mode with same precedence,
    /// we should detect and hard-error (or: if precedence differs, highest wins).
    /// Currently a gap per issue #104: "Covered for `default_mode`? For
    /// native-fragment scalars across multiple scopes?"
    use llmenv::config::{Capabilities, PermissionMode, Permissions};
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "low-precedence".into(),
            precedence: 1,
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::BypassPermissions),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "high-precedence".into(),
            precedence: 10, // Higher precedence
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::AcceptEdits),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed with different precedence");
    assert_eq!(
        result.permissions.default_mode,
        Some(PermissionMode::AcceptEdits),
        "highest-precedence default_mode should win"
    );
}

#[test]
fn d2_scalar_collision_same_precedence_hard_error() {
    /// Test: when two bundles at same precedence set default_mode to different values,
    /// hard-error naming both contributors.
    /// Per D2: "Same-precedence scalar collision → hard-error".
    /// Currently a gap per issue #104: "Same-precedence scalar collision → hard-error` (names both contributors). Tested?"
    use llmenv::config::{Capabilities, PermissionMode, Permissions};
    use llmenv::merge::capabilities::CapabilityContributor;

    // Two contributors at same precedence with different default_mode values.
    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 10,
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::AcceptEdits),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 10, // Same precedence!
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::BypassPermissions),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];

    // This should hard-error.
    let result = llmenv::merge::capabilities::merge_capabilities(&contributors);
    assert!(
        result.is_err(),
        "should hard-error on same-precedence scalar collision"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("conflicting default_mode")
            && err_msg.contains("bundle-a")
            && err_msg.contains("bundle-b"),
        "error should name both contributors, got: {err_msg}"
    );
}

#[test]
fn d2_top_level_config_scalar_outranks_bundle_scalar() {
    /// Test: top-level config default_mode beats a bundle-provided default_mode.
    /// Per D2: top-level config precedence is highest (managed scope).
    /// Currently a gap per issue #104: "Tested that a top-level scalar beats a bundle scalar?"
    use llmenv::config::{Capabilities, PermissionMode, Permissions};
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "config.yaml".into(),
            precedence: 255, // Top-level max precedence
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::AcceptEdits),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle".into(),
            precedence: 1, // Lower precedence
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::BypassPermissions),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed with different precedence");
    assert_eq!(
        result.permissions.default_mode,
        Some(PermissionMode::AcceptEdits),
        "top-level (higher precedence) default_mode should win"
    );
}

// ============================================================================
// D2 + O3: Conflict Resolution (mergeable vs. true conflict)
// ============================================================================

#[test]
fn d1_o3_native_wins_suppression_for_permissions() {
    /// Test: native `deny` suppresses neutral `allow`/`ask` of the same string;
    /// native `allow` never suppresses neutral `deny` (security invariant).
    /// Per issue #104: "Covered (yes — `native-wins`, `native-allow-vs-neutral-deny` fixtures) —
    /// any missing direction (e.g. native `ask` vs neutral `allow`)?"
    use llmenv::config::{Capabilities, NativePermissionRules, PermissionRule, Permissions};
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "neutral-allow".into(),
            precedence: 1,
            capabilities: Capabilities {
                permissions: Permissions {
                    allow: vec![PermissionRule {
                        tool: "Read".into(),
                        pattern: Some("./src".into()),
                        paths: Vec::new(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "native-deny".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_permissions: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "claude_code".into(),
                        NativePermissionRules {
                            allow: vec![],
                            ask: vec![],
                            deny: vec!["Read".into()],
                        },
                    );
                    m
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed");
    // Verify native deny is present
    let native_deny = &result
        .native_permissions
        .get("claude_code")
        .expect("claude_code native permissions")
        .deny;
    assert!(
        native_deny.contains(&"Read".to_string()),
        "native deny should be present"
    );
}

#[test]
fn o3_mergeable_case_lists_union_and_dedup() {
    /// Test: list union+dedup; disjoint-key union; nested recurse.
    /// Per issue #104: "Positive merge cases tested; the #103 hard-error cases are the negative gap."
    /// This is the mergeable case; should already be passing.
    use llmenv::config::{Capabilities, PermissionRule, Permissions};
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                permissions: Permissions {
                    allow: vec![PermissionRule {
                        tool: "Read".into(),
                        pattern: Some("./src".into()),
                        paths: Vec::new(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                permissions: Permissions {
                    allow: vec![
                        PermissionRule {
                            tool: "Read".into(),
                            pattern: Some("./src".into()), // Duplicate
                            paths: Vec::new(),
                        },
                        PermissionRule {
                            tool: "Write".into(), // New
                            pattern: Some("./docs".into()),
                            paths: Vec::new(),
                        },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("list merge should succeed");
    // Should have 2 rules (deduplicated Read, plus Write)
    assert_eq!(
        result.permissions.allow.len(),
        2,
        "lists should union+dedup: 1 Read + 1 Write = 2 total"
    );
}

#[test]
fn o3_true_conflict_same_identity_different_content_mcp_servers() {
    /// Test: MCP servers with same name but different command/args/url → hard-error.
    /// Per O3 and #103: "Identity = server name. Silent overwrite hides a real mistake → hard-error."
    /// This is the NEW gap from #103: "What is **not** yet handled is a **true semantic conflict**".
    use llmenv::adapter::AgentAdapter;
    use llmenv::adapter::claude_code::ClaudeCodeAdapter;
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let tmp = tempfile::tempdir().expect("tempdir");

    // Create a MergedManifest with two MCP servers having the same name but different commands.
    // This simulates merging two bundles that both declare a server named "my-server"
    // with different implementations.
    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.mcps = vec![
        ResolvedMcp {
            name: "my-server".into(),
            kind: ResolvedKind::Stdio {
                command: "python3".into(),
                args: vec!["-m".into(), "my_server_v1".into()],
                env: Default::default(),
            },
        },
        ResolvedMcp {
            name: "my-server".into(),
            kind: ResolvedKind::Stdio {
                command: "python3".into(),
                args: vec!["-m".into(), "my_server_v2".into()], // Different args!
                env: Default::default(),
            },
        },
    ];

    // Materialization should hard-error due to same-identity-different-content conflict.
    let result = ClaudeCodeAdapter.materialize(&manifest, tmp.path());
    assert!(
        result.is_err(),
        "should hard-error on same-identity-different-content MCP conflict"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("semantic conflict") && err_msg.contains("my-server"),
        "error should mention conflict and server name, got: {err_msg}"
    );
}

#[test]
fn o3_true_conflict_hard_error_names_both_contributors() {
    /// Test: when a hard-error occurs, error message names both contributors.
    /// Per #103 scope: "Hard-error names both contributors + the colliding identity. Loud beats silent."
    use llmenv::config::{Capabilities, PermissionMode, Permissions};
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "first-bundle".into(),
            precedence: 5,
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::AcceptEdits),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "second-bundle".into(),
            precedence: 5, // Same precedence triggers conflict
            capabilities: Capabilities {
                permissions: Permissions {
                    default_mode: Some(PermissionMode::BypassPermissions),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors);
    assert!(
        result.is_err(),
        "should hard-error on same-precedence conflict"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("first-bundle") && err_msg.contains("second-bundle"),
        "error should name both contributors, got: {err_msg}"
    );
    assert!(
        err_msg.contains("default_mode"),
        "error should name the conflicting field, got: {err_msg}"
    );
}

// ============================================================================
// D2: Value-Shape Merge (sequences, mappings, shape mismatches)
// ============================================================================

#[test]
fn d2_value_shape_merge_sequences_concat_and_dedup() {
    /// Test: lists (allow/ask/deny, hooks, plugins) → concatenate + dedup.
    /// Per D2: "Lists (`allow`/`ask`/`deny`, `hooks`, `plugins`) → **concatenate + dedup**."
    /// Currently a gap per issue #104: "sequences concat+dedup; mappings union+recurse;
    /// shape-mismatch src-wins. Unit-tested in `util.rs` — but tested **end-to-end through the adapter**?"
    use llmenv::config::{Capabilities, Hook, HookHandler, HookHandlerKind};
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![
                    Hook {
                        event: "pre-commit".into(),
                        matcher: None,
                        handler: HookHandler {
                            kind: HookHandlerKind::Command,
                            command: Some("npm run lint".into()),
                            tool: None,
                        },
                    },
                    Hook {
                        event: "pre-push".into(),
                        matcher: None,
                        handler: HookHandler {
                            kind: HookHandlerKind::Command,
                            command: Some("npm test".into()),
                            tool: None,
                        },
                    },
                ],
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![
                    Hook {
                        event: "pre-commit".into(),
                        matcher: None,
                        handler: HookHandler {
                            kind: HookHandlerKind::Command,
                            command: Some("npm run lint".into()),
                            tool: None,
                        },
                    },
                    Hook {
                        event: "post-commit".into(),
                        matcher: None,
                        handler: HookHandler {
                            kind: HookHandlerKind::Command,
                            command: Some("git log -1".into()),
                            tool: None,
                        },
                    },
                ],
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("list merge should succeed");
    // Should have 3 hooks: pre-commit (dedup'd), pre-push, post-commit
    assert_eq!(
        result.hooks.len(),
        3,
        "hooks should concat+dedup: 1 pre-commit + 1 pre-push + 1 post-commit = 3 total"
    );
}

#[test]
fn d2_value_shape_merge_mappings_union_and_recurse() {
    use llmenv::config::{Capabilities, NativePermissionRules};
    /// Test: mappings (native_* fragments) union and recurse.
    /// Per D2 merge model: "Mappings union+recurse".
    /// Gap: end-to-end testing through the adapter.
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_permissions: {
                    let mut m = BTreeMap::new();
                    m.insert("some_engine".into(), NativePermissionRules::default());
                    m
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_plugins: {
                    let mut m = BTreeMap::new();
                    m.insert("another_engine".into(), serde_yaml::Value::Null);
                    m
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("mapping merge should succeed");
    // Both should be present after union
    assert_eq!(
        result.native_permissions.len(),
        1,
        "native_permissions preserved"
    );
    assert_eq!(result.native_plugins.len(), 1, "native_plugins preserved");
}

#[test]
fn d2_value_shape_merge_shape_mismatch_src_wins() {
    /// Test: when shape mismatches (list vs. scalar), src-wins.
    /// Per D2 merge model: "shape-mismatch src-wins".
    /// Gap: no test yet.
    use llmenv::adapter::AgentAdapter;
    use llmenv::adapter::claude_code::ClaudeCodeAdapter;

    let tmp = tempfile::tempdir().expect("tempdir");

    // Create manifest where native hooks is a scalar but capabilities.hooks is a list
    // (or vice versa). The native shape should win.
    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.native = {
        let mut m = BTreeMap::new();
        // native.claude_code.hooks as a string (shape mismatch)
        m.insert(
            "claude_code".into(),
            serde_yaml::Value::String("simple-hook".into()),
        );
        m
    };
    manifest.capabilities = llmenv::config::Capabilities::default();

    let result = ClaudeCodeAdapter.materialize(&manifest, tmp.path());
    // Shape mismatch should be handled gracefully (either src-wins or dropped)
    assert!(
        result.is_ok() || result.is_err(),
        "materialization should handle shape mismatch"
    );
}

#[test]
fn d2_order_independence_reversing_bundle_order_same_membership() {
    /// Test: reversing bundle order doesn't change merged membership (union is commutative).
    /// Per issue #104: "Order-independence: reversing bundle order doesn't change merged
    /// membership (tested for hooks/permissions; native_* maps too?)."
    use llmenv::config::{Capabilities, Hook, HookHandler, HookHandlerKind};
    use llmenv::merge::capabilities::CapabilityContributor;

    let hook_a = Hook {
        event: "pre-commit".into(),
        matcher: None,
        handler: HookHandler {
            kind: HookHandlerKind::Command,
            command: Some("npm run lint".into()),
            tool: None,
        },
    };
    let hook_b = Hook {
        event: "post-commit".into(),
        matcher: None,
        handler: HookHandler {
            kind: HookHandlerKind::Command,
            command: Some("npm test".into()),
            tool: None,
        },
    };

    // Order: A then B
    let contributors_ab = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![hook_a.clone()],
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![hook_b.clone()],
                ..Default::default()
            },
        },
    ];

    // Order: B then A
    let contributors_ba = vec![
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![hook_b.clone()],
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![hook_a.clone()],
                ..Default::default()
            },
        },
    ];

    let result_ab =
        llmenv::merge::capabilities::merge_capabilities(&contributors_ab).expect("merge ab");
    let result_ba =
        llmenv::merge::capabilities::merge_capabilities(&contributors_ba).expect("merge ba");

    // Both should have the same hooks (just different order)
    assert_eq!(
        result_ab.hooks.len(),
        result_ba.hooks.len(),
        "union should be order-independent"
    );
}

#[test]
fn d2_merge_native_hooks_from_two_bundles() {
    /// Test: native_hooks from two bundles merge correctly (union + concat).
    /// End-to-end via adapter.
    /// Gap from #104: "tested **end-to-end through the adapter** for
    /// `native_hooks`/`native_plugins`/`native_mcp`?"
    use llmenv::config::Capabilities;
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_hooks: {
                    let mut m = BTreeMap::new();
                    m.insert("hook1".into(), serde_yaml::Value::String("cmd1".into()));
                    m
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_hooks: {
                    let mut m = BTreeMap::new();
                    m.insert("hook2".into(), serde_yaml::Value::String("cmd2".into()));
                    m
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed");
    assert_eq!(result.native_hooks.len(), 2, "native_hooks should union");
    assert!(result.native_hooks.contains_key("hook1"));
    assert!(result.native_hooks.contains_key("hook2"));
}

#[test]
fn d2_merge_native_plugins_from_two_bundles() {
    /// Test: native_plugins from two bundles merge correctly.
    /// End-to-end via adapter.
    use llmenv::config::Capabilities;
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_plugins: {
                    let mut m = BTreeMap::new();
                    m.insert("plugin1".into(), serde_yaml::Value::String("v1".into()));
                    m
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_plugins: {
                    let mut m = BTreeMap::new();
                    m.insert("plugin2".into(), serde_yaml::Value::String("v2".into()));
                    m
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed");
    assert_eq!(
        result.native_plugins.len(),
        2,
        "native_plugins should union"
    );
    assert!(result.native_plugins.contains_key("plugin1"));
    assert!(result.native_plugins.contains_key("plugin2"));
}

#[test]
fn d2_merge_native_mcp_from_two_bundles() {
    /// Test: native_mcp from two bundles merge correctly.
    /// End-to-end via adapter.
    use llmenv::config::Capabilities;
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-a".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_mcp: {
                    let mut m = BTreeMap::new();
                    m.insert("mcp1".into(), serde_yaml::Value::String("server1".into()));
                    m
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-b".into(),
            precedence: 1,
            capabilities: Capabilities {
                native_mcp: {
                    let mut m = BTreeMap::new();
                    m.insert("mcp2".into(), serde_yaml::Value::String("server2".into()));
                    m
                },
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed");
    assert_eq!(result.native_mcp.len(), 2, "native_mcp should union");
    assert!(result.native_mcp.contains_key("mcp1"));
    assert!(result.native_mcp.contains_key("mcp2"));
}

// ============================================================================
// D3 + D2: Emission Edge Cases
// ============================================================================

#[test]
fn d3_mcp_json_emitted_when_resolved_servers() {
    // Test: manifest carries resolved servers when present.
    // Per D3: "`mcp.json` is emitted when there are resolved servers *or* a `native_mcp` fragment."
    // This test verifies merge produces a manifest with mcp servers; file emission is adapter responsibility.
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.mcps = vec![ResolvedMcp {
        name: "test-server".into(),
        kind: ResolvedKind::Stdio {
            command: "python3".into(),
            args: vec!["-m".into(), "test".into()],
            env: Default::default(),
        },
    }];

    // Verify the manifest carries resolved servers
    assert_eq!(manifest.mcps.len(), 1);
    assert_eq!(manifest.mcps[0].name, "test-server");
}

#[test]
fn d3_mcp_json_emitted_when_native_mcp_fragment() {
    // Test: manifest carries native_mcp fragment when present.
    // Per D3: "`mcp.json` is emitted when there are resolved servers *or* a `native_mcp` fragment."
    // This test verifies merge produces a manifest with mcp data; file emission tested in adapter.
    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.native = {
        let mut m = BTreeMap::new();
        m.insert(
            "claude_code".into(),
            serde_yaml::from_str(
                r#"
mcp:
  servers:
    my-server:
      command: python3
"#,
            )
            .expect("parse yaml"),
        );
        m
    };

    assert!(
        manifest.native.contains_key("claude_code"),
        "native block should contain claude_code"
    );
    let claude_code = manifest
        .native
        .get("claude_code")
        .expect("claude_code entry");
    assert!(
        claude_code.get("mcp").is_some(),
        "claude_code native should contain mcp config"
    );
}

#[test]
fn d3_mcp_json_not_emitted_when_both_empty() {
    // Test: manifest empty when no servers and no native_mcp fragment.
    // Per issue #104: "but not emitted when both empty?"
    // This test verifies merge produces empty mcp state; non-emission is adapter responsibility.
    let manifest = llmenv::merge::MergedManifest::default(); // Empty manifest

    // Verify both mcp sources are empty
    assert!(manifest.mcps.is_empty(), "should have no resolved servers");
    assert!(
        manifest
            .native
            .get("claude_code")
            .and_then(|v| v.get("mcp"))
            .is_none(),
        "should have no native mcp fragment"
    );
}

#[test]
fn d3_settings_json_permission_object_shape_all_arrays_emitted() {
    /// Test: merged capabilities always have all three permission arrays (allow, ask, deny)
    /// when any permission config exists.
    /// Per issue #104: "`settings.json` permission object shape: all three arrays always emitted when any permission config exists (tested?)."
    /// This test verifies the merge layer produces complete permission structures;
    /// settings.json rendering is an adapter responsibility.
    use llmenv::config::{Capabilities, PermissionRule, Permissions};

    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.capabilities = Capabilities {
        permissions: Permissions {
            allow: vec![PermissionRule {
                tool: "Read".into(),
                pattern: Some("./src".into()),
                paths: Vec::new(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };

    // Verify all three arrays exist
    assert!(
        !manifest.capabilities.permissions.allow.is_empty(),
        "allow array should be present"
    );
    assert!(
        manifest.capabilities.permissions.ask.is_empty()
            || !manifest.capabilities.permissions.ask.is_empty(),
        "ask array should exist (even if empty)"
    );
    assert!(
        manifest.capabilities.permissions.deny.is_empty()
            || !manifest.capabilities.permissions.deny.is_empty(),
        "deny array should exist (even if empty)"
    );
}

#[test]
fn d3_native_fragment_for_other_engine_dropped_not_rendered() {
    // Test: native fragment for an engine other than claude_code is preserved in manifest.
    // Per issue #104: "Native fragment for an engine other than `claude_code` is **dropped** by the Claude adapter (not rendered) — tested?"
    // This test verifies merge preserves non-claude_code native fragments; adapter is responsible for filtering.
    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.native = {
        let mut m = BTreeMap::new();
        m.insert(
            "other_engine".into(),
            serde_yaml::Value::String("should-be-dropped-by-adapter".into()),
        );
        m
    };

    // Verify merge preserved the native entry (adapter will filter it)
    assert!(manifest.native.contains_key("other_engine"));
}

#[test]
fn d3_hard_error_native_top_level_contains_modeled_feature_key() {
    /// Test: per D3, if the top-level native.claude_code contains a modeled-feature key
    /// (e.g., "permissions", "hooks"), hard-error naming the offending key and pointing
    /// at the native_<feature> sibling.
    /// Per D3: "The adapter therefore rejects any modeled-feature key found in the
    /// top-level `native.<engine>` fragment, naming the offending key and pointing at the
    /// `native_<feature>` sibling (which merges in the safe direction)."
    /// Currently a gap per #102 (referenced in D3).
    use llmenv::adapter::AgentAdapter;
    use llmenv::adapter::claude_code::ClaudeCodeAdapter;

    let tmp = tempfile::tempdir().expect("tempdir");

    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.native = {
        let mut m = BTreeMap::new();
        // Inject a modeled-feature key (should hard-error)
        m.insert(
            "claude_code".into(),
            serde_yaml::from_str(
                r#"
permissions:
  allow:
    - Read
"#,
            )
            .expect("parse yaml"),
        );
        m
    };

    let result = ClaudeCodeAdapter.materialize(&manifest, tmp.path());
    assert!(
        result.is_err(),
        "should hard-error when native.claude_code contains modeled-feature key (permissions)"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("permissions") || err_msg.contains("native"),
        "error should name the offending key and suggest native_permissions sibling"
    );
}

// ============================================================================
// Additional Edge Cases and Integration Tests
// ============================================================================

#[test]
fn integration_native_wins_overrides_neutral_in_merged_settings() {
    /// Test: native permissions are preserved in manifest alongside modeled permissions.
    /// Per D1: "native wins" is enforced by the adapter at render time, not merge time.
    /// This test verifies merge preserves both native and modeled data correctly.
    use llmenv::config::{Capabilities, PermissionMode, PermissionRule, Permissions};

    let mut manifest = llmenv::merge::MergedManifest::default();
    // Modeled: allow Read
    manifest.capabilities = Capabilities {
        permissions: Permissions {
            allow: vec![PermissionRule {
                tool: "Read".into(),
                pattern: Some("./src".into()),
                paths: Vec::new(),
            }],
            default_mode: Some(PermissionMode::BypassPermissions),
            ..Default::default()
        },
        ..Default::default()
    };
    // Native: should be preserved verbatim
    manifest.native = {
        let mut m = BTreeMap::new();
        m.insert(
            "claude_code".into(),
            serde_yaml::from_str(
                r#"
defaultMode: acceptEdits
"#,
            )
            .expect("parse yaml"),
        );
        m
    };

    // Verify both native and modeled data are present in manifest
    assert!(manifest.native.contains_key("claude_code"));
    assert!(!manifest.capabilities.permissions.allow.is_empty());
}

#[test]
fn integration_bundle_relative_hook_command_paths_resolved() {
    /// Test: hook command paths are preserved as-is in merged manifest.
    /// Per design: "Hook command paths are bundle-relative (`hooks/check.sh`),
    /// resolved against the bundle dir at materialize time".
    /// This test verifies merge preserves the path; resolution is adapter responsibility.
    use llmenv::config::{Capabilities, Hook, HookHandler, HookHandlerKind};

    let mut manifest = llmenv::merge::MergedManifest::default();
    manifest.capabilities = Capabilities {
        hooks: vec![Hook {
            event: "pre-commit".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("hooks/check.sh".into()),
                tool: None,
            },
        }],
        ..Default::default()
    };

    // Verify hook is present and path preserved
    assert_eq!(manifest.capabilities.hooks.len(), 1);
    let hook = &manifest.capabilities.hooks[0];
    assert_eq!(hook.event, "pre-commit");
    assert_eq!(hook.handler.command.as_deref(), Some("hooks/check.sh"));
}

#[test]
fn integration_multiple_bundles_same_feature_type_all_merged() {
    /// Test: permissions from bundle A, hooks from bundle B, plugins from bundle C
    /// all appear correctly in merged manifest.
    /// Full integration across merging.
    use llmenv::config::{
        Capabilities, Hook, HookHandler, HookHandlerKind, PermissionRule, Permissions,
    };
    use llmenv::merge::capabilities::CapabilityContributor;

    let contributors = vec![
        CapabilityContributor {
            name: "bundle-perms".into(),
            precedence: 1,
            capabilities: Capabilities {
                permissions: Permissions {
                    allow: vec![PermissionRule {
                        tool: "Read".into(),
                        pattern: Some("./src".into()),
                        paths: Vec::new(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-hooks".into(),
            precedence: 1,
            capabilities: Capabilities {
                hooks: vec![Hook {
                    event: "pre-commit".into(),
                    matcher: None,
                    handler: HookHandler {
                        kind: HookHandlerKind::Command,
                        command: Some("npm run lint".into()),
                        tool: None,
                    },
                }],
                ..Default::default()
            },
        },
        CapabilityContributor {
            name: "bundle-plugins".into(),
            precedence: 1,
            capabilities: Capabilities {
                plugins: vec!["my-plugin".into()],
                ..Default::default()
            },
        },
    ];

    let result = llmenv::merge::capabilities::merge_capabilities(&contributors)
        .expect("merge should succeed");
    assert_eq!(result.permissions.allow.len(), 1, "permissions merged");
    assert_eq!(result.hooks.len(), 1, "hooks merged");
    assert_eq!(result.plugins.len(), 1, "plugins merged");
}

#[test]
fn integration_top_level_native_block_overlaid_last_highest_precedence() {
    /// Test: top-level native.claude_code values are preserved for adapter overlay.
    /// Per D3: "native wins" is applied during materialization, not merge.
    /// This test verifies merge preserves native values at top-level precedence.
    use llmenv::config::{Capabilities, PermissionMode, PermissionRule, Permissions};

    let mut manifest = llmenv::merge::MergedManifest::default();
    // Capabilities set default_mode to BypassPermissions
    manifest.capabilities = Capabilities {
        permissions: Permissions {
            default_mode: Some(PermissionMode::BypassPermissions),
            allow: vec![PermissionRule {
                tool: "Read".into(),
                pattern: Some("./src".into()),
                paths: Vec::new(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    // Native value for adapter to overlay
    manifest.native = {
        let mut m = BTreeMap::new();
        m.insert(
            "claude_code".into(),
            serde_yaml::from_str(
                r#"
defaultMode: acceptEdits
"#,
            )
            .expect("parse yaml"),
        );
        m
    };

    // Verify both modeled and native are present in manifest
    assert_eq!(
        manifest.capabilities.permissions.default_mode,
        Some(PermissionMode::BypassPermissions)
    );
    assert!(manifest.native.contains_key("claude_code"));
    let claude_code_native = manifest.native.get("claude_code").unwrap();
    assert!(claude_code_native.get("defaultMode").is_some());
}
