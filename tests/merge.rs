use llmenv::merge::{BundleRef, merge};
use std::path::PathBuf;

fn bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
    }
}

#[test]
fn merges_two_bundles_with_provenance() {
    let m = merge(&[bundle("base"), bundle("rust-defaults")]).expect("merge");
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
    let m = merge(&[]).expect("merge");
    assert!(m.agents_md.is_empty());
    assert!(m.files.is_empty());
}

#[test]
fn agents_md_order_follows_bundle_order() {
    let a = merge(&[bundle("base"), bundle("rust-defaults")]).expect("merge");
    let b = merge(&[bundle("rust-defaults"), bundle("base")]).expect("merge");
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

    let m = merge(&[
        BundleRef {
            name: "a".into(),
            path: a.clone(),
        },
        BundleRef {
            name: "b".into(),
            path: b.clone(),
        },
    ])
    .expect("merge");
    let by_rel: BTreeMap<_, _> = m.files.iter().collect();
    let key = std::path::PathBuf::from("skills/x.md");
    let abs = by_rel.get(&key).expect("collision key");
    let contents = std::fs::read_to_string(abs).expect("read");
    assert_eq!(contents, "from b", "later bundle should win");
}
