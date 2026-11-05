use super::{reject_cycle, task_path};
use tempfile::TempDir;

#[test]
fn a_corrupt_ancestor_fails_the_parent_change() {
    let dir = TempDir::new().expect("tempdir");
    let path = task_path(dir.path(), "ancestor");
    std::fs::create_dir_all(path.parent().expect("task dir")).expect("mkdir");
    std::fs::write(&path, "{not task json").expect("write corrupt ancestor");

    let err = reject_cycle(dir.path(), "child", "ancestor")
        .expect_err("a corrupt ancestor must not pass silently");

    let msg = format!("{err:#}");
    assert!(msg.contains("ancestor"), "must name the ancestor: {msg}");
}

#[test]
fn a_missing_ancestor_ends_the_walk_without_error() {
    let dir = TempDir::new().expect("tempdir");

    reject_cycle(dir.path(), "child", "absent")
        .expect("a missing ancestor is not a cycle and not a corrupt file");
}
