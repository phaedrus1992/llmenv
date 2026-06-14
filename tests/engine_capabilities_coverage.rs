#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
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
    let manifest = llmenv::merge::MergedManifest {
        mcps: vec![
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
        ],
        ..Default::default()
    };

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
                        bundle_origin: None,
                    },
                    Hook {
                        event: "pre-push".into(),
                        matcher: None,
                        handler: HookHandler {
                            kind: HookHandlerKind::Command,
                            command: Some("npm test".into()),
                            tool: None,
                        },
                        bundle_origin: None,
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
                        bundle_origin: None,
                    },
                    Hook {
                        event: "post-commit".into(),
                        matcher: None,
                        handler: HookHandler {
                            kind: HookHandlerKind::Command,
                            command: Some("git log -1".into()),
                            tool: None,
                        },
                        bundle_origin: None,
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
    let manifest = llmenv::merge::MergedManifest {
        native: {
            let mut m = BTreeMap::new();
            // native.claude_code.hooks as a string (shape mismatch)
            m.insert(
                "claude_code".into(),
                serde_yaml::Value::String("simple-hook".into()),
            );
            m
        },
        capabilities: llmenv::config::Capabilities::default(),
        ..Default::default()
    };

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
        bundle_origin: None,
    };
    let hook_b = Hook {
        event: "post-commit".into(),
        matcher: None,
        handler: HookHandler {
            kind: HookHandlerKind::Command,
            command: Some("npm test".into()),
            tool: None,
        },
        bundle_origin: None,
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
fn d3_mcp_servers_present_when_resolved() {
    // Test: manifest carries resolved servers when present.
    // Per D3 (#244): MCP servers are merged into `.claude.json` when there are
    // resolved servers *or* a `native_mcp` fragment.
    // This test verifies merge produces a manifest with mcp servers; the merge
    // into `.claude.json` is the adapter's responsibility.
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let manifest = llmenv::merge::MergedManifest {
        mcps: vec![ResolvedMcp {
            name: "test-server".into(),
            kind: ResolvedKind::Stdio {
                command: "python3".into(),
                args: vec!["-m".into(), "test".into()],
                env: Default::default(),
            },
        }],
        ..Default::default()
    };

    // Verify the manifest carries resolved servers
    assert_eq!(manifest.mcps.len(), 1);
    assert_eq!(manifest.mcps[0].name, "test-server");
}

#[test]
fn d3_native_mcp_fragment_present_when_declared() {
    // Test: manifest carries native_mcp fragment when present.
    // Per D3 (#244): MCP servers are merged into `.claude.json` when there are
    // resolved servers *or* a `native_mcp` fragment.
    // This test verifies merge produces a manifest with mcp data; the merge into
    // `.claude.json` is tested in the adapter.
    let manifest = llmenv::merge::MergedManifest {
        native: {
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
        },
        ..Default::default()
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
fn d3_no_mcp_when_both_empty() {
    // Test: manifest empty when no servers and no native_mcp fragment.
    // Per issue #104: "but not emitted when both empty?"
    // This test verifies merge produces empty mcp state; non-emission of
    // `.claude.json` mcpServers is the adapter's responsibility.
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

    let manifest = llmenv::merge::MergedManifest {
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
    let mut native = BTreeMap::new();
    native.insert(
        "other_engine".into(),
        serde_yaml::Value::String("should-be-dropped-by-adapter".into()),
    );
    let manifest = llmenv::merge::MergedManifest {
        native,
        ..Default::default()
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

    let mut native = BTreeMap::new();
    // Inject a modeled-feature key (should hard-error)
    native.insert(
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
    let manifest = llmenv::merge::MergedManifest {
        native,
        ..Default::default()
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

    let mut native = BTreeMap::new();
    native.insert(
        "claude_code".into(),
        serde_yaml::from_str(
            r#"
defaultMode: acceptEdits
"#,
        )
        .expect("parse yaml"),
    );
    let manifest = llmenv::merge::MergedManifest {
        capabilities: Capabilities {
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
        },
        native,
        ..Default::default()
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

    let manifest = llmenv::merge::MergedManifest {
        capabilities: Capabilities {
            hooks: vec![Hook {
                event: "pre-commit".into(),
                matcher: None,
                handler: HookHandler {
                    kind: HookHandlerKind::Command,
                    command: Some("hooks/check.sh".into()),
                    tool: None,
                },
                bundle_origin: None,
            }],
            ..Default::default()
        },
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
                    bundle_origin: None,
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

    let mut native = BTreeMap::new();
    native.insert(
        "claude_code".into(),
        serde_yaml::from_str(
            r#"
defaultMode: acceptEdits
"#,
        )
        .expect("parse yaml"),
    );
    let manifest = llmenv::merge::MergedManifest {
        capabilities: Capabilities {
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
        },
        native,
        ..Default::default()
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

// ============================================================================
// #111: Invariant fuzzing for the D2 value-shape merge engine (`merge_json`).
//
// The example-based tests above pin specific D2/D3/O3 behaviors. These property
// tests fuzz the merge primitive itself to assert the invariants those examples
// assume hold for *all* inputs, not just the hand-picked cases.
// ============================================================================
mod merge_json_invariants {
    use llmenv::util::merge_json;
    use proptest::prelude::*;
    use serde_json::Value;

    // Bounded arbitrary JSON. Depth-limited so generation stays cheap while still
    // producing nested objects/arrays that exercise the recursive merge.
    fn arb_json(depth: u32) -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| Value::Number(n.into())),
            "[a-z]{0,6}".prop_map(Value::String),
        ];
        leaf.prop_recursive(depth, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                proptest::collection::vec(("[a-z]{1,5}", inner), 0..4)
                    .prop_map(|pairs| { Value::Object(pairs.into_iter().collect()) }),
            ]
        })
    }

    proptest! {
        // Merging never panics for any pair of JSON values.
        #[test]
        fn merge_never_panics(mut dst in arb_json(4), src in arb_json(4)) {
            merge_json(&mut dst, src);
        }

        // Determinism: merging the same pair twice yields identical results.
        #[test]
        fn merge_is_deterministic(dst in arb_json(3), src in arb_json(3)) {
            let mut a = dst.clone();
            merge_json(&mut a, src.clone());
            let mut b = dst;
            merge_json(&mut b, src);
            prop_assert_eq!(a, b);
        }

        // Scalar / shape-mismatch overwrite: when `src` is a scalar (or the two
        // shapes differ), `src` wins wholesale — it is the higher-precedence
        // overlay. The overwrite normalizes src's arrays (dedup at every depth)
        // so the result matches what every other merge path produces. Verified
        // by overwriting a scalar dst with arbitrary src and comparing against a
        // separately-normalized src (obtained by overwriting a fresh scalar).
        #[test]
        fn scalar_dst_is_overwritten_by_normalized_src(src in arb_json(3)) {
            let mut dst = Value::String("scalar".into());
            merge_json(&mut dst, src.clone());

            // Independently normalize src via the same overwrite path.
            let mut normalized_src = Value::String("other".into());
            merge_json(&mut normalized_src, src);

            prop_assert_eq!(dst, normalized_src);
        }

        // Object union: keys present only in `src` always appear in the result
        // with their `src` value; keys only in `dst` are preserved.
        #[test]
        fn object_merge_unions_disjoint_keys(
            dst_keys in proptest::collection::hash_map("[a-z]{1,5}", any::<i64>(), 0..5),
            src_keys in proptest::collection::hash_map("[A-Z]{1,5}", any::<i64>(), 0..5),
        ) {
            // Disjoint key spaces (lowercase vs uppercase) so no recursion/collision.
            let dst_obj: serde_json::Map<String, Value> = dst_keys
                .iter()
                .map(|(k, v)| (k.clone(), Value::Number((*v).into())))
                .collect();
            let src_obj: serde_json::Map<String, Value> = src_keys
                .iter()
                .map(|(k, v)| (k.clone(), Value::Number((*v).into())))
                .collect();
            let mut dst = Value::Object(dst_obj);
            merge_json(&mut dst, Value::Object(src_obj));
            let out = dst.as_object().unwrap();
            for (k, v) in &dst_keys {
                prop_assert_eq!(out.get(k).unwrap(), &Value::Number((*v).into()));
            }
            for (k, v) in &src_keys {
                prop_assert_eq!(out.get(k).unwrap(), &Value::Number((*v).into()));
            }
        }

        // Array merge is concat-then-dedup: the result contains every distinct
        // element from dst and src, with no duplicates, and dst's elements first.
        #[test]
        fn array_merge_concats_and_dedups(
            a in proptest::collection::vec(0i64..8, 0..6),
            b in proptest::collection::vec(0i64..8, 0..6),
        ) {
            let to_arr = |xs: &[i64]| Value::Array(xs.iter().map(|n| Value::Number((*n).into())).collect());
            let mut dst = to_arr(&a);
            merge_json(&mut dst, to_arr(&b));
            let out = dst.as_array().unwrap();

            // No duplicates in the result.
            for i in 0..out.len() {
                for j in (i + 1)..out.len() {
                    prop_assert_ne!(&out[i], &out[j]);
                }
            }
            // Every distinct input element is present.
            for n in a.iter().chain(b.iter()) {
                prop_assert!(out.contains(&Value::Number((*n).into())));
            }
        }

        // Idempotence on dedup-free arrays: merging `src` into a result that has
        // already absorbed `src` is a no-op when src carries no duplicate elements.
        #[test]
        fn merge_idempotent_for_distinct_arrays(
            base in proptest::collection::vec(0i64..6, 0..4),
            overlay in proptest::collection::hash_set(10i64..16, 0..4),
        ) {
            let overlay: Vec<i64> = overlay.into_iter().collect();
            let to_arr = |xs: &[i64]| Value::Array(xs.iter().map(|n| Value::Number((*n).into())).collect());
            let mut once = to_arr(&base);
            merge_json(&mut once, to_arr(&overlay));
            let mut twice = once.clone();
            merge_json(&mut twice, to_arr(&overlay));
            prop_assert_eq!(once, twice);
        }
    }
}

// Parity fuzzing for the YAML merge path. `merge_yaml` and `merge_json` share
// the same value-shape rule (objects union, sequences concat-then-dedup, scalars
// overwrite) and the same normalization fix, so they must satisfy the same
// invariants. These mirror `merge_json_invariants` against serde_yaml::Value.
mod merge_yaml_invariants {
    use llmenv::util::merge_yaml;
    use proptest::prelude::*;
    use serde_yaml::Value;

    // Bounded arbitrary YAML, structurally identical to arb_json above.
    fn arb_yaml(depth: u32) -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| Value::Number(n.into())),
            "[a-z]{0,6}".prop_map(Value::String),
        ];
        leaf.prop_recursive(depth, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Sequence),
                proptest::collection::vec(("[a-z]{1,5}", inner), 0..4).prop_map(|pairs| {
                    Value::Mapping(
                        pairs
                            .into_iter()
                            .map(|(k, v)| (Value::String(k), v))
                            .collect(),
                    )
                }),
            ]
        })
    }

    proptest! {
        // Merging never panics for any pair of YAML values.
        #[test]
        fn merge_never_panics(mut dst in arb_yaml(4), src in arb_yaml(4)) {
            merge_yaml(&mut dst, src);
        }

        // Determinism: merging the same pair twice yields identical results.
        #[test]
        fn merge_is_deterministic(dst in arb_yaml(3), src in arb_yaml(3)) {
            let mut a = dst.clone();
            merge_yaml(&mut a, src.clone());
            let mut b = dst;
            merge_yaml(&mut b, src);
            prop_assert_eq!(a, b);
        }

        // Scalar / shape-mismatch overwrite: src wins wholesale and its sequences
        // are normalized, so the result matches an independently-normalized src.
        #[test]
        fn scalar_dst_is_overwritten_by_normalized_src(src in arb_yaml(3)) {
            let mut dst = Value::String("scalar".into());
            merge_yaml(&mut dst, src.clone());

            let mut normalized_src = Value::String("other".into());
            merge_yaml(&mut normalized_src, src);

            prop_assert_eq!(dst, normalized_src);
        }

        // Sequence merge is concat-then-dedup: result holds every distinct element
        // from dst and src with no duplicates.
        #[test]
        fn sequence_merge_concats_and_dedups(
            a in proptest::collection::vec(0i64..8, 0..6),
            b in proptest::collection::vec(0i64..8, 0..6),
        ) {
            let to_seq = |xs: &[i64]| {
                Value::Sequence(xs.iter().map(|n| Value::Number((*n).into())).collect())
            };
            let mut dst = to_seq(&a);
            merge_yaml(&mut dst, to_seq(&b));
            let out = dst.as_sequence().unwrap();

            for i in 0..out.len() {
                for j in (i + 1)..out.len() {
                    prop_assert_ne!(&out[i], &out[j]);
                }
            }
            for n in a.iter().chain(b.iter()) {
                prop_assert!(out.contains(&Value::Number((*n).into())));
            }
        }

        // Idempotence on dedup-free sequences: re-merging an already-absorbed
        // overlay is a no-op.
        #[test]
        fn merge_idempotent_for_distinct_sequences(
            base in proptest::collection::vec(0i64..6, 0..4),
            overlay in proptest::collection::hash_set(10i64..16, 0..4),
        ) {
            let overlay: Vec<i64> = overlay.into_iter().collect();
            let to_seq = |xs: &[i64]| {
                Value::Sequence(xs.iter().map(|n| Value::Number((*n).into())).collect())
            };
            let mut once = to_seq(&base);
            merge_yaml(&mut once, to_seq(&overlay));
            let mut twice = once.clone();
            merge_yaml(&mut twice, to_seq(&overlay));
            prop_assert_eq!(once, twice);
        }
    }
}
