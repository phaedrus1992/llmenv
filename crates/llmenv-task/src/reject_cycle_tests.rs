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
fn a_missing_ancestor_fails_the_parent_change_with_its_fix() {
    let dir = TempDir::new().expect("tempdir");

    let err = reject_cycle(dir.path(), "child", "absent")
        .expect_err("a missing ancestor must not end the walk silently");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("'absent'"),
        "must name the missing task: {msg}"
    );
    assert!(msg.contains("Fix:"), "must state the fix: {msg}");
}

/// Writes a stored task `slug` whose parent is `parent`, with the fields the store needs.
fn write_task_with_parent(dir: &std::path::Path, slug: &str, parent: &str) {
    let path = task_path(dir, slug);
    std::fs::create_dir_all(path.parent().expect("task dir")).expect("mkdir");
    let body = serde_json::json!({
        "slug": slug,
        "title": slug,
        "parent": parent,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    std::fs::write(&path, body.to_string()).expect("write task");
}

#[test]
fn a_loop_among_the_ancestors_fails_the_parent_change_with_its_fix() {
    let dir = TempDir::new().expect("tempdir");
    write_task_with_parent(dir.path(), "loop-a", "loop-b");
    write_task_with_parent(dir.path(), "loop-b", "loop-a");

    let err = reject_cycle(dir.path(), "child", "loop-a")
        .expect_err("a loop above the new parent must not pass silently");

    let msg = format!("{err:#}");
    assert!(msg.contains("loop"), "must say the ancestors loop: {msg}");
    assert!(msg.contains("Fix:"), "must state the fix: {msg}");
}
