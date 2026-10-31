//! Undo after a failed apply: the made folders and links are removed, and their targets stay.

#![expect(
    clippy::expect_used,
    reason = "test setup fails loudly on a bad fixture"
)]

use super::{CONFIG, Setup};
use crate::onboarding::apply::{make_files, undo};

/// The undo after a failure removes the nested folders of a shipped bundle, newest first.
#[test]
fn undo_removes_the_nested_folders_of_a_shipped_bundle() {
    let s = Setup::new(CONFIG, "bundles: [{name: objc-dev, shipped: true}]");
    let plan = crate::onboarding::plan::Plan::load(&s.path("plan.yaml")).unwrap();
    let mut made = Vec::new();
    make_files(&plan, s.dir.path(), &mut made).unwrap();
    assert!(
        s.path("bundles/objc-dev/skills/arc-ownership-check/scripts")
            .is_dir()
    );
    let note = undo(&made);
    assert!(note.contains("the files it made are removed"), "{note}");
    assert!(!s.path("bundles/objc-dev").exists());
}

// #3116: `is_dir()` follows a link to a folder, so undo chose `remove_dir`, which refuses a
// link, and the link stayed. The link goes; its target folder stays.
#[cfg(unix)]
#[test]
fn undo_removes_a_link_to_a_folder_and_keeps_the_folder() {
    let dir = tempfile::tempdir().expect("test");
    let target = dir.path().join("real");
    std::fs::create_dir(&target).expect("test");
    let link = dir.path().join("made-link");
    std::os::unix::fs::symlink(&target, &link).expect("test");

    let report = undo(std::slice::from_ref(&link));

    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the link must be removed: {report}"
    );
    assert!(target.is_dir(), "the folder the link points to must stay");
}
