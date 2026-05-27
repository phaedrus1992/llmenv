use llmenv::config::Capabilities;
use llmenv::merge::{BundleRef, merge};
use std::path::PathBuf;

fn bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
        precedence: 1,
    }
}

#[test]
fn merges_two_bundles_with_provenance() {
    let m = merge(
        &Capabilities::default(),
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
    let m = merge(&Capabilities::default(), &[]).expect("merge");
    assert!(m.agents_md.is_empty());
    assert!(m.files.is_empty());
    assert!(m.capabilities.is_empty());
}

#[test]
fn bundle_without_bundle_yaml_contributes_no_capabilities() {
    // rust-defaults has only AGENTS.md + skills, no bundle.yaml.
    let m = merge(&Capabilities::default(), &[bundle("rust-defaults")]).expect("merge");
    assert!(m.capabilities.is_empty());
}

#[test]
fn bundle_yaml_is_read_into_merged_capabilities() {
    let m = merge(&Capabilities::default(), &[bundle("with-capabilities")]).expect("merge");
    let caps = &m.capabilities;
    assert_eq!(
        caps.permissions.default_mode,
        Some(llmenv::config::PermissionMode::AcceptEdits)
    );
    assert_eq!(caps.permissions.allow.len(), 1);
    assert_eq!(caps.permissions.deny.len(), 1);
    assert_eq!(caps.permissions.native["claude_code"].deny.len(), 1);
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
    let m = merge(&top, &[bundle("with-capabilities")]).expect("merge");
    // 1 from top-level + 1 from the bundle = 2 allow rules.
    assert_eq!(m.capabilities.permissions.allow.len(), 2);
}

#[test]
fn agents_md_order_follows_bundle_order() {
    let a = merge(
        &Capabilities::default(),
        &[bundle("base"), bundle("rust-defaults")],
    )
    .expect("merge");
    let b = merge(
        &Capabilities::default(),
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
