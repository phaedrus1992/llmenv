//! Tests for owner-only folders and files.

use super::*;

// A file that exists with a looser mode is tightened by the write, and a second write of the
// same bytes then changes nothing (#3024).
#[cfg(unix)]
#[test]
fn write_owner_only_tightens_a_loose_file_and_then_skips() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f");
    std::fs::write(&path, b"old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write_owner_only(&path, b"new").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let before = crate::write_stats::snapshot();
    write_owner_only(&path, b"new").unwrap();
    let delta = crate::write_stats::snapshot().since(before);
    assert!(
        delta.skipped >= 1,
        "the second write of the same bytes is skipped"
    );
}

// The log is born owner-only, a looser existing log is tightened, and a symlink is refused.
#[cfg(unix)]
#[test]
fn open_append_owner_only_makes_tightens_and_refuses_a_link() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let fresh = dir.path().join("fresh.log");
    writeln!(open_append_owner_only(&fresh).unwrap(), "one").unwrap();
    writeln!(open_append_owner_only(&fresh).unwrap(), "two").unwrap();
    assert_eq!(mode_of(&fresh), 0o600);
    assert_eq!(
        std::fs::read_to_string(&fresh).unwrap(),
        "one\ntwo\n",
        "it appends"
    );

    let loose = dir.path().join("loose.log");
    std::fs::write(&loose, b"x").unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
    open_append_owner_only(&loose).unwrap();
    assert_eq!(mode_of(&loose), 0o600);

    let link = dir.path().join("link.log");
    std::os::unix::fs::symlink(&fresh, &link).unwrap();
    assert!(
        open_append_owner_only(&link).is_err(),
        "a symlink must be refused"
    );
}

#[cfg(unix)]
#[test]
fn create_dir_owner_only_is_0o700_from_creation() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("nested").join("store");
    create_dir_owner_only(&dir).unwrap();

    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "dir must be born owner-only, got {mode:o}");
}

#[test]
fn create_dir_owner_only_is_idempotent_on_existing_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("store");
    create_dir_owner_only(&dir).unwrap();
    create_dir_owner_only(&dir).unwrap();
    assert!(dir.is_dir());
}

// A directory created before this hardening shipped (older llmenv
// version, a caller using bare create_dir_all, a permissive umask) must
// still end up owner-only the next time something calls
// create_dir_owner_only on it -- not just directories it creates fresh.
#[cfg(unix)]
#[test]
fn create_dir_owner_only_hardens_a_preexisting_looser_dir() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("store");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    create_dir_owner_only(&dir).unwrap();

    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "pre-existing looser dir must be hardened, got {mode:o}"
    );
}

// #3137: a chmod by path follows a link, so a planted link at the folder would change the
// folder it points to. The target must keep its mode, and the call must fail.
#[cfg(unix)]
#[test]
fn create_dir_owner_only_refuses_a_symlink_and_leaves_its_target_alone() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("shared");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = tmp.path().join("store");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    assert!(create_dir_owner_only(&link).is_err());

    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o755,
        "the link target must keep its mode, got {mode:o}"
    );
}

#[cfg(unix)]
// An unreadable folder (mode 0o300) left by an earlier bug must still self-heal. The open that
// the chmod needs requires read permission, so the self-heal has to work without it.
#[cfg(unix)]
#[test]
fn create_dir_owner_only_heals_an_unreadable_dir() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("store");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();

    let result = create_dir_owner_only(&dir);
    // Restore a mode that lets the tempdir clean up, whatever the result.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    result.unwrap();
}

#[test]
fn write_owner_only_sets_mode_0o600() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("secret");
    write_owner_only(&path, b"sensitive").expect("write");
    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode();
    // Group/other bits must be clear — file is owner-only.
    assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
    let body = std::fs::read(&path).expect("read");
    assert_eq!(body, b"sensitive");
}

#[cfg(unix)]
#[test]
fn write_owner_only_refuses_a_symlink_and_leaves_its_target_alone() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let target = tmp.path().join("target");
    let link = tmp.path().join("link");
    std::fs::write(&target, b"keep").expect("seed target");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let err = write_owner_only(&link, b"attacker").expect_err("symlink must be refused");
    let eloop = std::io::Error::from_raw_os_error(rustix::io::Errno::LOOP.raw_os_error());
    assert_eq!(
        err.kind(),
        eloop.kind(),
        "a symlink at the final component must fail with ELOOP: {err}"
    );
    assert!(
        err.to_string().contains(&link.display().to_string()),
        "{err}"
    );
    assert_eq!(std::fs::read(&target).expect("read target"), b"keep");
}

#[cfg(unix)]
#[test]
fn write_owner_only_fails_at_once_on_a_fifo() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    assert!(write_owner_only(&path, b"x").is_err());
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn the_executable_writer_sets_0o700_on_a_new_and_an_existing_file() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("run.sh");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    write_owner_only_executable(&path, b"a").unwrap();
    assert_eq!(mode(&path), 0o700);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    write_owner_only_executable(&path, b"b").unwrap();
    assert_eq!(mode(&path), 0o700, "an existing file gets the bit");
    assert_eq!(std::fs::read(&path).unwrap(), b"b");
}

#[cfg(unix)]
#[test]
fn the_like_writer_takes_the_executable_bit_from_its_source() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let script = tmp.path().join("src.sh");
    std::fs::write(&script, b"x").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();
    let plain = tmp.path().join("src.md");
    std::fs::write(&plain, b"x").unwrap();
    assert!(is_executable(&script).unwrap() && !is_executable(&plain).unwrap());
    write_owner_only_like(&tmp.path().join("a"), b"x", &script).unwrap();
    write_owner_only_like(&tmp.path().join("b"), b"x", &plain).unwrap();
    assert_eq!(mode(&tmp.path().join("a")), 0o700);
    assert_eq!(mode(&tmp.path().join("b")), 0o600);
    let err = is_executable(&tmp.path().join("missing")).unwrap_err();
    assert!(err.to_string().contains("missing"), "{err}");
}

#[test]
fn write_owner_only_truncates_existing_file() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("file");
    write_owner_only(&path, b"longer content").expect("write1");
    write_owner_only(&path, b"short").expect("write2");
    let body = std::fs::read(&path).expect("read");
    assert_eq!(body, b"short");
}
