use super::*;
use crate::merge::MergedManifest;
use proptest::prelude::*;
use std::collections::BTreeMap;

/// A skill or a hook can run a bundle script directly, so each render mode keeps its
/// executable bit, and a plain file stays plain.
#[cfg(unix)]
#[test]
fn a_bundle_script_keeps_its_executable_bit_in_each_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let script = tmp.path().join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\n").expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let plain = tmp.path().join("notes.md");
    std::fs::write(&plain, b"x").expect("write plain");
    let mut files = BTreeMap::new();
    files.insert(PathBuf::from("skills/s/scripts/run.sh"), script);
    files.insert(PathBuf::from("skills/s/notes.md"), plain);
    let m = MergedManifest {
        files,
        ..Default::default()
    };
    // Normal mode writes in place on every call, so its second pass writes over files that
    // already have the bit.
    for (mode, pass) in [
        (HashingMode::Strict, 1),
        (HashingMode::Normal, 1),
        (HashingMode::Normal, 2),
    ] {
        let cache = tmp.path().join(format!("cache-{mode:?}"));
        let out = materialize_with_mode(&m, &cache, mode, "")
            .expect("render")
            .path;
        let mode = format!("{mode:?} pass {pass}");
        let mode_of = |rel: &str| {
            std::fs::metadata(out.join(rel))
                .expect("rendered file")
                .permissions()
                .mode()
        };
        assert_ne!(mode_of("skills/s/scripts/run.sh") & 0o100, 0, "{mode}");
        assert_eq!(mode_of("skills/s/notes.md") & 0o111, 0, "{mode}");
    }
}

/// #149: a bundle file with a `..` component must be rejected, not joined
/// into staging (which would escape the cache dir).
#[test]
fn materialize_rejects_path_traversal_in_files() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, b"x").expect("write src");
    let cache = tmp.path().join("cache");

    let mut files = BTreeMap::new();
    files.insert(PathBuf::from("../escape.txt"), src);
    let m = MergedManifest {
        files,
        ..Default::default()
    };
    let err = materialize(&m, &cache).expect_err("must reject traversal");
    assert!(
        err.to_string().contains("traversal"),
        "unexpected error: {err}"
    );
}

/// #2657: a file at the content-hash path is not a cached folder. Strict mode must not return
/// it as a hit. It may fail, or it may produce a real directory at that path.
#[test]
fn strict_mode_does_not_reuse_a_file_at_the_hash_path() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, b"x").expect("write src");
    let mut files = BTreeMap::new();
    files.insert(PathBuf::from("a.txt"), src);
    let m = MergedManifest {
        files,
        ..Default::default()
    };
    let hash = cache::hash_manifest(&m).expect("hash");
    let folder = cache::folder_name(HashingMode::Strict, "", &hash);
    std::fs::write(cache.join(&folder), b"not a folder").expect("squatter file");

    let result = materialize_with_mode(&m, &cache, HashingMode::Strict, "");
    assert!(
        result.as_ref().is_err() || result.as_ref().is_ok_and(|r| r.path.is_dir()),
        "a file at the hash path was returned as a cached folder: {result:?}"
    );
}

/// #149: an absolute `rel` would escape staging via Path::join's
/// "absolute argument discards base" rule. Must be rejected.
#[test]
fn materialize_rejects_absolute_path_in_files() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, b"x").expect("write src");
    let cache = tmp.path().join("cache");

    let mut files = BTreeMap::new();
    files.insert(PathBuf::from("/etc/llmenv-escape.txt"), src);
    let m = MergedManifest {
        files,
        ..Default::default()
    };
    let err = materialize(&m, &cache).expect_err("must reject absolute path");
    assert!(
        err.to_string().contains("traversal"),
        "unexpected error: {err}"
    );
}

/// `write_in_place` (loose/normal mode) re-renders `m.files` into a
/// folder that persists across calls — the agent's live config dir. If a
/// prior render's destination entry gets replaced by a symlink (e.g. a
/// malicious or misbehaving plugin), the next render must replace the
/// symlink rather than write through it, matching the hardening
/// `src/materialize/inherit.rs` already applies for the same class of
/// TOCTOU bug (#1341, extended here per #1423).
#[cfg(unix)]
#[test]
fn write_in_place_does_not_write_through_a_symlinked_destination() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src = tmp.path().join("bundle-file.txt");
    std::fs::write(&src, b"bundle-content").expect("write src");
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).expect("create cache");

    let victim = tmp.path().join("victim.txt");
    std::fs::write(&victim, b"must-not-be-touched").expect("write victim");

    let mut files = BTreeMap::new();
    files.insert(PathBuf::from("out.txt"), src);
    let m = MergedManifest {
        files,
        ..Default::default()
    };

    let rendered = materialize(&m, &cache).expect("first render");
    let dest_file = rendered.path.join("out.txt");
    std::fs::remove_file(&dest_file).expect("remove first render's output");
    std::os::unix::fs::symlink(&victim, &dest_file).expect("plant symlink");

    materialize(&m, &cache).expect("second render");

    assert!(
        !std::fs::symlink_metadata(&dest_file)
            .expect("stat dest")
            .file_type()
            .is_symlink(),
        "the planted symlink must be replaced, not written through"
    );
    assert_eq!(
        std::fs::read(&dest_file).expect("read dest"),
        b"bundle-content"
    );
    assert_eq!(
        std::fs::read(&victim).expect("read victim"),
        b"must-not-be-touched",
        "the symlink's target must be untouched"
    );
}

/// The stronger case #1427 closes: a symlinked *directory* component
/// anywhere in a bundle file's relative path must not be followed either
/// — `create_dir_all` on a path containing one would happily resolve
/// through it, landing the write inside the symlink's target instead of
/// under `dest`.
#[cfg(unix)]
#[test]
fn write_in_place_does_not_follow_a_symlinked_directory_component() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let src = tmp.path().join("bundle-file.txt");
    std::fs::write(&src, b"bundle-content").expect("write src");
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).expect("create cache");

    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("create outside");

    let mut files = BTreeMap::new();
    files.insert(PathBuf::from("hooks/out.txt"), src);
    let m = MergedManifest {
        files,
        ..Default::default()
    };

    // First render creates `hooks/` for real; swap it for a symlink to a
    // directory outside `dest` before the next render.
    let rendered = materialize(&m, &cache).expect("first render");
    let hooks_dir = rendered.path.join("hooks");
    std::fs::remove_dir_all(&hooks_dir).expect("remove first render's hooks dir");
    std::os::unix::fs::symlink(&outside, &hooks_dir).expect("plant symlinked directory");

    let err = materialize(&m, &cache).expect_err("must refuse to follow the symlinked dir");
    let _ = err;

    assert!(
        !outside.join("out.txt").exists(),
        "the write must not land inside the symlinked directory's target"
    );
}

// #341: prune_empty_dirs — root is never removed regardless of tree shape.
proptest! {
    #[test]
    fn prune_empty_dirs_never_removes_root(
        dirs in proptest::collection::vec("[a-z]{1,6}", 0..8_usize)
    ) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).expect("create root");
        for d in &dirs {
            std::fs::create_dir_all(root.join(d)).expect("create subdir");
        }
        prune_empty_dirs(&root).expect("prune");
        prop_assert!(root.exists(), "root must survive prune");
    }
}

// #341: prune_empty_dirs — files in subdirs are preserved.
proptest! {
    #[test]
    fn prune_empty_dirs_preserves_files(
        dir in "[a-z]{1,6}",
        filename in "[a-z]{1,6}"
    ) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path().join("out");
        let subdir = root.join(&dir);
        std::fs::create_dir_all(&subdir).expect("create subdir");
        let file = subdir.join(&filename);
        std::fs::write(&file, b"content").expect("write file");
        prune_empty_dirs(&root).expect("prune");
        prop_assert!(file.exists(), "file must survive prune");
        prop_assert!(subdir.exists(), "non-empty dir must survive prune");
    }
}

// #341: prune_empty_dirs — idempotent: second run produces same result.
proptest! {
    #[test]
    fn prune_empty_dirs_is_idempotent(
        dirs in proptest::collection::vec("[a-z]{1,6}", 0..6_usize)
    ) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).expect("create root");
        for d in &dirs {
            std::fs::create_dir_all(root.join(d)).expect("create subdir");
        }
        prune_empty_dirs(&root).expect("first prune");
        prune_empty_dirs(&root).expect("second prune");
        prop_assert!(root.exists(), "root must still exist after second prune");
    }
}

/// A link planted at the destination is replaced, and its target is not written (#1427).
#[cfg(unix)]
#[test]
fn write_bundle_file_does_not_write_through_a_link_at_the_destination() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("source.sh");
    std::fs::write(&source, b"echo new\n").expect("write the source");
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"keep me").expect("write the victim");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("make the root");
    std::os::unix::fs::symlink(&victim, root.join("out.sh")).expect("plant a link");
    write_bundle_file(&root, Path::new("out.sh"), &source).expect("write the bundle file");
    assert_eq!(std::fs::read(&victim).expect("read the victim"), b"keep me");
    let copy = std::fs::read(root.join("out.sh")).expect("read the copy");
    assert_eq!(copy, b"echo new\n");
}

/// A bundle script keeps its executable bit when it is written under a new folder (#3203).
#[cfg(unix)]
#[test]
fn write_bundle_file_copies_content_and_keeps_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("hook.sh");
    std::fs::write(&source, b"echo hi\n").expect("write the source");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("make the root");
    write_bundle_file(&root, Path::new("sub/out.sh"), &source).expect("copy the file");
    let out = root.join("sub/out.sh");
    assert_eq!(std::fs::read(&out).expect("read the copy"), b"echo hi\n");
    let mode = std::fs::metadata(&out)
        .expect("stat the copy")
        .permissions()
        .mode();
    assert_ne!(mode & 0o100, 0, "the copy must stay executable: {mode:o}");
}
