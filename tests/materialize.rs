#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use llmenv::config::HashingMode;
use llmenv::materialize::{cache, materialize_with_mode};
use llmenv::merge::{BundleRef, merge};
use tempfile::tempdir;

/// The empty-selection shape, used by the strict/normal helpers below.
fn empty_shape() -> String {
    cache::shape(&BTreeSet::new(), &BTreeSet::new())
}

/// Strict-mode materialize: content-addressed folders. The crate's `materialize`
/// convenience wrapper now defaults to normal mode (#246), so the dedup/skew
/// tests below pin strict explicitly.
fn materialize_strict(
    m: &llmenv::merge::MergedManifest,
    root: &std::path::Path,
) -> std::path::PathBuf {
    materialize_with_mode(m, root, HashingMode::Strict, &empty_shape())
        .expect("materialize strict")
        .path
}

fn fixture_bundle(name: &str) -> BundleRef {
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
fn materializes_deterministically() {
    let tmp = tempdir().expect("tempdir");
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    let p1 = materialize_strict(&m, tmp.path());
    let p2 = materialize_strict(&m, tmp.path());
    assert_eq!(p1, p2, "same manifest hashes to same path");
    // materialize copies raw bundle files only — rules text is rendered by
    // the per-agent adapter, not written here.
    assert!(!p1.join("AGENTS.md").exists());
    assert!(p1.join("skills/hello/SKILL.md").exists());
}

#[test]
fn different_manifests_produce_different_dirs() {
    let tmp = tempdir().expect("tempdir");
    let m_base = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge base");
    let m_both = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base"), fixture_bundle("rust-defaults")],
    )
    .expect("merge both");
    let p1 = materialize_strict(&m_base, tmp.path());
    let p2 = materialize_strict(&m_both, tmp.path());
    assert_ne!(p1, p2);
}

#[test]
fn normal_mode_reuses_one_folder_across_manifests() {
    // #246: normal mode names the folder after <version_mm>/<shape>, not the
    // content hash. Two different manifests sharing the same selection shape
    // therefore render into the SAME folder (last-writer-wins), unlike strict
    // mode above.
    let tmp = tempdir().expect("tempdir");
    let m_base = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge base");
    let m_both = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base"), fixture_bundle("rust-defaults")],
    )
    .expect("merge both");
    let shape = empty_shape();
    let r1 = materialize_with_mode(&m_base, tmp.path(), HashingMode::Normal, &shape)
        .expect("materialize base");
    let r2 = materialize_with_mode(&m_both, tmp.path(), HashingMode::Normal, &shape)
        .expect("materialize both");
    assert_eq!(r1.path, r2.path, "normal mode reuses the same folder");
    assert_ne!(r1.hash, r2.hash, "but the content hash still differs");
}

#[test]
fn no_tmp_stage_dir_after_success() {
    let tmp = tempdir().expect("tempdir");
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    let _ = materialize_strict(&m, tmp.path());
    for entry in std::fs::read_dir(tmp.path()).expect("read_dir") {
        let p = entry.expect("entry").path();
        assert!(
            p.extension().is_none_or(|e| e != "tmp"),
            "staging dir leaked: {}",
            p.display()
        );
    }
}

#[test]
fn gc_removes_old_entries_and_keeps_fresh_ones() {
    let tmp = tempdir().expect("tempdir");
    // Old entry: backdate its mtime by 30 days.
    let old = tmp.path().join("old");
    std::fs::create_dir_all(&old).expect("mkdir old");
    let marker = old.join("marker");
    std::fs::write(&marker, "x").expect("write marker");
    let old_time = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30);
    set_mtime(&marker, old_time);
    set_mtime(&old, old_time);

    // Fresh entry: today.
    let fresh = tmp.path().join("fresh");
    std::fs::create_dir_all(&fresh).expect("mkdir fresh");
    std::fs::write(fresh.join("marker"), "y").expect("write marker");

    let report = cache::gc(tmp.path(), Duration::from_secs(60 * 60 * 24 * 7)).expect("gc");
    assert!(!old.exists(), "old entry should have been removed");
    assert!(fresh.exists(), "fresh entry should remain");
    assert_eq!(report.kept, 1);
    assert_eq!(report.removed.len(), 1);
}

#[test]
fn gc_removes_tmp_stage_dirs_regardless_of_age() {
    let tmp = tempdir().expect("tempdir");
    let stage = tmp.path().join("abcd.tmp");
    std::fs::create_dir_all(&stage).expect("mkdir stage");
    std::fs::write(stage.join("marker"), "x").expect("write marker");

    let report = cache::gc(tmp.path(), Duration::from_secs(60 * 60 * 24 * 365)).expect("gc");
    assert!(!stage.exists(), "stage dir should always be GC'd");
    assert_eq!(report.removed.len(), 1);
}

#[test]
fn hash_is_unambiguous_across_field_boundaries() {
    // Without length prefixing, manifest A and B below would hash identically
    // because `agents_md || files[].rel || files[].bytes` concatenates to the
    // same bytes. With length prefixing they must differ.
    use llmenv::materialize::cache::hash_manifest;
    use llmenv::merge::MergedManifest;
    use std::collections::BTreeMap;

    let tmp = tempdir().expect("tempdir");
    let f_de = tmp.path().join("de");
    let f_e = tmp.path().join("e");
    std::fs::write(&f_de, b"FG").expect("write de");
    std::fs::write(&f_e, b"FG").expect("write e");

    let mut a_files = BTreeMap::new();
    a_files.insert(PathBuf::from("DE"), f_de.clone());
    let a = MergedManifest {
        agents_md: "ABC".into(),
        files: a_files,
        ..Default::default()
    };

    let mut b_files = BTreeMap::new();
    b_files.insert(PathBuf::from("E"), f_e.clone());
    let b = MergedManifest {
        agents_md: "ABCD".into(),
        files: b_files,
        ..Default::default()
    };

    let ha = hash_manifest(&a).expect("hash a");
    let hb = hash_manifest(&b).expect("hash b");
    assert_ne!(ha, hb, "hash must distinguish field boundaries");
}

#[test]
fn gc_on_missing_root_is_noop() {
    let tmp = tempdir().expect("tempdir");
    let missing = tmp.path().join("nope");
    let report = cache::gc(&missing, Duration::from_secs(1)).expect("gc");
    assert!(report.removed.is_empty());
    assert_eq!(report.kept, 0);
}

fn set_mtime(p: &std::path::Path, t: SystemTime) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(p)
        .or_else(|_| std::fs::File::open(p))
        .expect("open");
    f.set_modified(t).expect("set_modified");
}
