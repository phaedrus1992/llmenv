#![expect(clippy::expect_used, reason = "test scaffolding")]
//! `llmenv clean` runs every cleanup: the stale cache folders that `prune` removed, and the
//! sweeps that `doctor --gc` ran. `prune` and `doctor --gc` are deprecated forms (#2917).
//!
//! These tests do not age a `state/tmp` file to removal. The sweep judges age by the later of
//! mtime and ctime (#3195), and a test cannot move a ctime back. Removal of an aged entry is
//! covered by the unit tests of `tmp_sweep`, which use the mtime-only rule.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use support::isolated_llmenv_cmd;

const PRUNE_DEPRECATED: &str = "llmenv prune is deprecated; use llmenv clean";
const DOCTOR_GC_DEPRECATED: &str = "llmenv doctor --gc is deprecated; use llmenv clean";

/// One item for each share of the cleanup.
struct Fixture {
    dir: tempfile::TempDir,
    /// An orphaned staging folder in the cache. The `prune` share removes it.
    orphan_stage: PathBuf,
    /// A fresh file in `state/tmp`. No share removes it, because it is not old.
    tmp_file: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = "cache:\n  cache_dir: ~/cache\n  cache_retention_hours: 168\n";
    fs::write(dir.path().join("config.yaml"), config).expect("write config");
    let orphan_stage = dir.path().join("cache/claude-code/abcd.tmp");
    fs::create_dir_all(&orphan_stage).expect("mkdir stage");
    let tmp = dir.path().join("tmp");
    fs::create_dir_all(&tmp).expect("mkdir tmp");
    let tmp_file = tmp.join("scratch.txt");
    fs::write(&tmp_file, b"x").expect("write scratch");
    Fixture {
        dir,
        orphan_stage,
        tmp_file,
    }
}

fn run(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = isolated_llmenv_cmd(dir)
        .args(args)
        .output()
        .expect("run llmenv");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The cache and `state/tmp` paths in the output are resolved, so compare by file name.
fn lists(stderr: &str, path: &Path) -> bool {
    let name = path.file_name().expect("file name").to_string_lossy();
    stderr
        .lines()
        .any(|l| l.contains("would remove") && l.ends_with(name.as_ref()))
}

#[test]
fn a_dry_run_lists_the_orphan_stage_and_removes_nothing() {
    let f = fixture();
    let (ok, stderr) = run(f.dir.path(), &["clean", "--dry-run"]);
    assert!(ok, "clean --dry-run failed: {stderr}");
    assert!(lists(&stderr, &f.orphan_stage), "no prune share: {stderr}");
    assert!(
        !lists(&stderr, &f.tmp_file),
        "a fresh tmp file is listed: {stderr}"
    );
    assert!(f.orphan_stage.exists() && f.tmp_file.exists());
    assert!(!stderr.contains("deprecated"), "{stderr}");
}

#[test]
fn clean_removes_the_orphan_stage_and_keeps_a_fresh_tmp_file() {
    let f = fixture();
    let (ok, stderr) = run(f.dir.path(), &["clean"]);
    assert!(ok, "clean failed: {stderr}");
    assert!(!f.orphan_stage.exists(), "the orphan stage is still there");
    assert!(f.tmp_file.exists(), "clean removed a fresh tmp file");
}

#[test]
fn prune_runs_clean_and_prints_the_deprecation_line() {
    let f = fixture();
    let (ok, stderr) = run(f.dir.path(), &["prune", "--dry-run"]);
    assert!(ok, "prune --dry-run failed: {stderr}");
    assert!(stderr.lines().any(|l| l == PRUNE_DEPRECATED), "{stderr}");
    assert!(lists(&stderr, &f.orphan_stage), "{stderr}");
    assert!(f.orphan_stage.exists() && f.tmp_file.exists());
}

#[test]
fn doctor_gc_keeps_its_sweeps_and_prints_the_deprecation_line() {
    let f = fixture();
    let (_, stderr) = run(f.dir.path(), &["doctor", "--gc"]);
    assert!(
        stderr.lines().any(|l| l == DOCTOR_GC_DEPRECATED),
        "{stderr}"
    );
    assert!(
        f.orphan_stage.exists(),
        "doctor --gc took over the prune share"
    );
    assert!(f.tmp_file.exists(), "doctor --gc removed a fresh tmp file");
}

#[test]
fn doctor_without_gc_removes_nothing() {
    let f = fixture();
    let (_, stderr) = run(f.dir.path(), &["doctor"]);
    assert!(f.orphan_stage.exists() && f.tmp_file.exists(), "{stderr}");
    assert!(!stderr.contains("deprecated"), "{stderr}");
}

#[test]
fn all_and_older_than_are_refused_together() {
    let f = fixture();
    let (ok, stderr) = run(f.dir.path(), &["clean", "--all", "--older-than", "1d"]);
    assert!(!ok, "clean accepted --all with --older-than");
    assert!(stderr.contains("--all"), "{stderr}");
    assert!(f.orphan_stage.exists() && f.tmp_file.exists());
}

#[test]
fn a_failed_sweep_fails_clean_after_the_other_removals() {
    let f = fixture();
    let config = "cache:\n  cache_dir: ~/cache\n  cache_retention_hours: 168\nbundle:\n  - name: work\n    when: [work]\n";
    fs::write(f.dir.path().join("config.yaml"), config).expect("write config");
    fs::create_dir_all(f.dir.path().join("bundles/work")).expect("mkdir bundle");
    fs::write(
        f.dir.path().join("bundles/work/bundle.yaml"),
        "features: [not, a, mapping]\n",
    )
    .expect("write bundle");
    fs::create_dir_all(f.dir.path().join("codebase-memory")).expect("mkdir cbm");

    let (ok, stderr) = run(f.dir.path(), &["clean"]);

    assert!(!ok, "clean exited 0 with a skipped sweep: {stderr}");
    assert!(
        stderr.contains("could not be checked or removed"),
        "{stderr}"
    );
    assert!(!f.orphan_stage.exists(), "the prune share did not run");
}

#[test]
fn a_bad_older_than_value_removes_nothing() {
    let f = fixture();
    let (ok, stderr) = run(f.dir.path(), &["clean", "--older-than", "14dayz"]);
    assert!(!ok, "clean accepted a bad duration");
    assert!(stderr.contains("--older-than"), "{stderr}");
    assert!(f.orphan_stage.exists() && f.tmp_file.exists());
}
