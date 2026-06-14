#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
use llmenv::config::Capabilities;
use llmenv::merge::{BundleRef, merge};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
        precedence: 1,
    }
}

fn empty_native() -> BTreeMap<String, serde_yaml::Value> {
    BTreeMap::new()
}

#[test]
fn merges_two_bundles_with_provenance() {
    let m = merge(
        &Capabilities::default(),
        &empty_native(),
        &[bundle("base"), bundle("rust-defaults")],
    )
    .expect("merge");
    assert!(m.agents_md.contains("<!-- # from bundle: base -->"));
    assert!(
        m.agents_md
            .contains("<!-- # from bundle: rust-defaults -->")
    );
    // base/skills/hello/SKILL.md + rust-defaults/skills/clippy/SKILL.md
    assert_eq!(m.files.len(), 2);
    let keys: Vec<String> = m
        .files
        .keys()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert!(keys.iter().any(|k| k.ends_with("skills/hello/SKILL.md")));
    assert!(keys.iter().any(|k| k.ends_with("skills/clippy/SKILL.md")));
}

#[test]
fn empty_bundle_list_yields_empty_manifest() {
    let m = merge(&Capabilities::default(), &empty_native(), &[]).expect("merge");
    assert!(m.agents_md.is_empty());
    assert!(m.files.is_empty());
    assert!(m.capabilities.is_empty());
}

#[test]
fn bundle_without_bundle_yaml_contributes_no_capabilities() {
    // rust-defaults has only AGENTS.md + skills, no bundle.yaml.
    let m = merge(
        &Capabilities::default(),
        &empty_native(),
        &[bundle("rust-defaults")],
    )
    .expect("merge");
    assert!(m.capabilities.is_empty());
}

#[test]
fn bundle_yaml_is_read_into_merged_capabilities() {
    let m = merge(
        &Capabilities::default(),
        &empty_native(),
        &[bundle("with-capabilities")],
    )
    .expect("merge");
    let caps = &m.capabilities;
    assert_eq!(
        caps.permissions.default_mode,
        Some(llmenv::config::PermissionMode::AcceptEdits)
    );
    assert_eq!(caps.permissions.allow.len(), 1);
    assert_eq!(caps.permissions.deny.len(), 1);
    assert_eq!(caps.native_permissions["claude_code"].deny.len(), 1);
    assert_eq!(caps.hooks.len(), 1);
    assert_eq!(caps.plugins, vec!["superpowers:superpowers".to_string()]);
}

#[test]
fn top_level_capabilities_merge_with_bundle_fragments() {
    use llmenv::config::{PermissionRule, Permissions};
    let top = Capabilities {
        permissions: Permissions {
            allow: vec![PermissionRule {
                tool: "Read".into(),
                pattern: Some("./docs".into()),
                paths: Vec::new(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let m = merge(&top, &empty_native(), &[bundle("with-capabilities")]).expect("merge");
    // 1 from top-level + 1 from the bundle = 2 allow rules.
    assert_eq!(m.capabilities.permissions.allow.len(), 2);
}

#[test]
fn agents_md_order_follows_bundle_order() {
    let a = merge(
        &Capabilities::default(),
        &empty_native(),
        &[bundle("base"), bundle("rust-defaults")],
    )
    .expect("merge");
    let b = merge(
        &Capabilities::default(),
        &empty_native(),
        &[bundle("rust-defaults"), bundle("base")],
    )
    .expect("merge");
    let a_base = a.agents_md.find("base").expect("base in a");
    let a_rust = a.agents_md.find("rust-defaults").expect("rust in a");
    assert!(a_base < a_rust);
    let b_rust = b.agents_md.find("rust-defaults").expect("rust in b");
    let b_base = b.agents_md.rfind("# from bundle: base").expect("base in b");
    assert!(b_rust < b_base);
}

#[test]
fn later_bundle_overwrites_on_path_collision() {
    use std::collections::BTreeMap;
    // Synthetic: create two transient bundles sharing skills/x.md.
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(a.join("skills")).expect("mkdir a");
    std::fs::create_dir_all(b.join("skills")).expect("mkdir b");
    std::fs::write(a.join("skills/x.md"), "from a").expect("write a");
    std::fs::write(b.join("skills/x.md"), "from b").expect("write b");

    let m = merge(
        &Capabilities::default(),
        &empty_native(),
        &[
            BundleRef {
                name: "a".into(),
                path: a.clone(),
                precedence: 1,
            },
            BundleRef {
                name: "b".into(),
                path: b.clone(),
                precedence: 1,
            },
        ],
    )
    .expect("merge");
    let by_rel: BTreeMap<_, _> = m.files.iter().collect();
    let key = std::path::PathBuf::from("skills/x.md");
    let abs = by_rel.get(&key).expect("collision key");
    let contents = std::fs::read_to_string(abs).expect("read");
    assert_eq!(contents, "from b", "later bundle should win");
}

#[test]
fn bundle_yaml_env_vars_are_merged_into_capabilities() {
    use std::collections::BTreeMap;
    let caps_a = Capabilities {
        env: BTreeMap::from([("A_VAR".into(), "a_value".into())]),
        ..Default::default()
    };

    let caps_b = Capabilities {
        env: BTreeMap::from([("B_VAR".into(), "b_value".into())]),
        ..Default::default()
    };

    let contrib_a = llmenv::merge::CapabilityContributor {
        name: "a".into(),
        precedence: 1,
        capabilities: caps_a,
    };
    let contrib_b = llmenv::merge::CapabilityContributor {
        name: "b".into(),
        precedence: 2,
        capabilities: caps_b,
    };

    let merged = llmenv::merge::merge_capabilities(&[contrib_a, contrib_b]).unwrap();
    assert_eq!(merged.env.get("A_VAR").map(|s| s.as_str()), Some("a_value"));
    assert_eq!(merged.env.get("B_VAR").map(|s| s.as_str()), Some("b_value"));
}

#[test]
fn bundle_env_vars_higher_precedence_wins() {
    use std::collections::BTreeMap;
    let caps_a = Capabilities {
        env: BTreeMap::from([("SHARED_VAR".into(), "a_value".into())]),
        ..Default::default()
    };

    let caps_b = Capabilities {
        env: BTreeMap::from([("SHARED_VAR".into(), "b_value".into())]),
        ..Default::default()
    };

    let contrib_a = llmenv::merge::CapabilityContributor {
        name: "a".into(),
        precedence: 1,
        capabilities: caps_a,
    };
    let contrib_b = llmenv::merge::CapabilityContributor {
        name: "b".into(),
        precedence: 2,
        capabilities: caps_b,
    };

    let merged = llmenv::merge::merge_capabilities(&[contrib_a, contrib_b]).unwrap();
    assert_eq!(
        merged.env.get("SHARED_VAR").map(|s| s.as_str()),
        Some("b_value"),
        "higher precedence (b) should win"
    );
}

// #355: same-precedence, same-value agreement is not an error.
#[test]
fn env_same_precedence_same_value_is_ok() {
    let caps_a = Capabilities {
        env: BTreeMap::from([("KEY".into(), "shared_value".into())]),
        ..Default::default()
    };
    let caps_b = Capabilities {
        env: BTreeMap::from([("KEY".into(), "shared_value".into())]),
        ..Default::default()
    };
    let contrib_a = llmenv::merge::CapabilityContributor {
        name: "a".into(),
        precedence: 3,
        capabilities: caps_a,
    };
    let contrib_b = llmenv::merge::CapabilityContributor {
        name: "b".into(),
        precedence: 3,
        capabilities: caps_b,
    };
    let merged = llmenv::merge::merge_capabilities(&[contrib_a, contrib_b]).unwrap();
    assert_eq!(
        merged.env.get("KEY").map(|s| s.as_str()),
        Some("shared_value"),
        "same-precedence same-value agreement must not error"
    );
}

// #355: same-precedence, different-value conflict is a hard error.
#[test]
fn env_same_precedence_conflict_is_an_error() {
    let caps_a = Capabilities {
        env: BTreeMap::from([("MY_VAR".into(), "value_a".into())]),
        ..Default::default()
    };
    let caps_b = Capabilities {
        env: BTreeMap::from([("MY_VAR".into(), "value_b".into())]),
        ..Default::default()
    };
    let contrib_a = llmenv::merge::CapabilityContributor {
        name: "bundle-a".into(),
        precedence: 2,
        capabilities: caps_a,
    };
    let contrib_b = llmenv::merge::CapabilityContributor {
        name: "bundle-b".into(),
        precedence: 2,
        capabilities: caps_b,
    };
    let err = llmenv::merge::merge_capabilities(&[contrib_a, contrib_b])
        .unwrap_err()
        .to_string();
    assert!(err.contains("conflicting env key"), "got: {err}");
    assert!(err.contains("MY_VAR"), "got: {err}");
    assert!(
        err.contains("bundle-a") && err.contains("bundle-b"),
        "got: {err}"
    );
}

// #355: conflict must mention the hint about resolving via higher-precedence scope.
#[test]
fn env_conflict_error_contains_resolution_hint() {
    let caps_a = Capabilities {
        env: BTreeMap::from([("CONFLICT_KEY".into(), "v1".into())]),
        ..Default::default()
    };
    let caps_b = Capabilities {
        env: BTreeMap::from([("CONFLICT_KEY".into(), "v2".into())]),
        ..Default::default()
    };
    let err = llmenv::merge::merge_capabilities(&[
        llmenv::merge::CapabilityContributor {
            name: "x".into(),
            precedence: 1,
            capabilities: caps_a,
        },
        llmenv::merge::CapabilityContributor {
            name: "y".into(),
            precedence: 1,
            capabilities: caps_b,
        },
    ])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("higher-precedence scope"),
        "error should hint at resolution; got: {err}"
    );
}
