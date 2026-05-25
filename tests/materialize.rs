use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use llme::materialize::{cache, materialize};
use llme::merge::{BundleRef, merge};
use tempfile::tempdir;

fn fixture_bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
    }
}

#[test]
fn materializes_deterministically() {
    let tmp = tempdir().expect("tempdir");
    let m = merge(&[fixture_bundle("base")]).expect("merge");
    let p1 = materialize(&m, tmp.path()).expect("materialize 1");
    let p2 = materialize(&m, tmp.path()).expect("materialize 2");
    assert_eq!(p1, p2, "same manifest hashes to same path");
    assert!(p1.join("AGENTS.md").exists());
    assert!(p1.join("skills/hello.md").exists());
}

#[test]
fn different_manifests_produce_different_dirs() {
    let tmp = tempdir().expect("tempdir");
    let m_base = merge(&[fixture_bundle("base")]).expect("merge base");
    let m_both =
        merge(&[fixture_bundle("base"), fixture_bundle("rust-defaults")]).expect("merge both");
    let p1 = materialize(&m_base, tmp.path()).expect("materialize base");
    let p2 = materialize(&m_both, tmp.path()).expect("materialize both");
    assert_ne!(p1, p2);
}

#[test]
fn no_tmp_stage_dir_after_success() {
    let tmp = tempdir().expect("tempdir");
    let m = merge(&[fixture_bundle("base")]).expect("merge");
    let _ = materialize(&m, tmp.path()).expect("materialize");
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
