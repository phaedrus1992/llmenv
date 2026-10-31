//! Owner write-bit repair for the cache tree, so a GC can remove a folder llmenv made.

use std::path::Path;

/// `remove_dir_all`, retried once after restoring the owner write bit on any
/// directory in the tree that lacks it.
///
/// `remove_dir_all` unlinks children *through* their parent directory, so a
/// directory with no owner write bit (a `0o555` left behind by some other tool
/// that used the cache as scratch space) makes its whole subtree undeletable
/// and fails with `PermissionDenied`. llmenv created that tree, so a cache GC
/// restoring write access to its own directories is expected behavior, not an
/// escalation — without it the cache grows forever with no way for the tool to
/// clean up after itself.
pub(super) fn remove_dir_all_forcing_writable(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(p) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Best-effort: if the chmod pass can't fix it either, the retry
            // surfaces the real error rather than the chmod's.
            restore_dir_write_bits(p);
            std::fs::remove_dir_all(p)
        }
        other => other,
    }
}

/// True when `open` refused `dir` because it is a link. macOS reports `ENOTDIR` for a link that
/// `O_NOFOLLOW` refuses, and Linux reports `ELOOP`, so both codes count, and only a real link
/// matches. A file under a file is not a link, so it still reaches the log.
#[cfg(unix)]
fn refused_as_link(e: &std::io::Error, dir: &Path) -> bool {
    let refusal = [rustix::io::Errno::LOOP, rustix::io::Errno::NOTDIR]
        .iter()
        .any(|errno| e.raw_os_error() == Some(errno.raw_os_error()));
    refusal && std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink())
}

/// Add the owner `rwx` bits to `dir` and every directory beneath it.
///
/// The walk holds each folder open and works from that handle. It lists the children of the
/// handle and opens each child relative to it, so no path is resolved again after the open. A
/// link swapped in for a folder is then refused, and a link is never chmod'd (#3203). Errors are
/// logged, not returned: this is a best-effort assist for the retry in
/// [`remove_dir_all_forcing_writable`], and the retry reports the failure that actually matters.
#[cfg(unix)]
fn restore_dir_write_bits(dir: &Path) {
    let fd = match crate::paths::dirfd::open_dir_nofollow(dir) {
        Ok(fd) => fd,
        // A missing folder has nothing to restore, and a link is never chmod'd. Both stay silent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) if refused_as_link(&e, dir) => return,
        Err(e) => {
            tracing::error!(
                "cannot read {} for cache cleanup: {e}. Fix: check the cache folder's \
                 permissions; its contents may not be removed.",
                dir.display()
            );
            return;
        }
    };
    restore_open_dir(std::fs::File::from(fd), dir);
}

/// Restore the owner bits of the open folder `handle`, then of each folder inside it.
///
/// `label` is the path of `handle`, used only in log lines.
#[cfg(unix)]
fn restore_open_dir(handle: std::fs::File, label: &Path) {
    use std::os::fd::AsFd as _;
    use std::os::unix::fs::PermissionsExt as _;

    let meta = match handle.metadata() {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!("cannot stat {} for cache cleanup: {e}", label.display());
            return;
        }
    };
    let mode = meta.permissions().mode();
    // 0o700: traverse + write + read, the minimum needed to unlink children.
    if mode & 0o700 != 0o700
        && let Err(e) = handle.set_permissions(std::fs::Permissions::from_mode(mode | 0o700))
    {
        tracing::error!(
            "cannot make {} writable for cache cleanup: {e}; cleanup of its contents may fail",
            label.display()
        );
    }
    let entries = match crate::paths::dirfd::read_dir_entries(handle.as_fd()) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!("cannot list {} during cache cleanup: {e}", label.display());
            return;
        }
    };
    for entry in entries.iter().filter(|entry| entry.is_dir()) {
        let child = label.join(&entry.name);
        match crate::paths::dirfd::open_dir_at(handle.as_fd(), &entry.name) {
            Ok(fd) => restore_open_dir(std::fs::File::from(fd), &child),
            // The child was removed after the listing, so there is nothing to restore.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::error!(
                "cannot open {} for cache cleanup: {e}. Fix: check the folder's permissions.",
                child.display()
            ),
        }
    }
}

#[cfg(not(unix))]
fn restore_dir_write_bits(_dir: &Path) {}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "a test: a failed setup must stop it")]
mod tests {
    use super::*;
    use crate::test_log_capture::capture_logs;

    #[test]
    fn a_path_that_cannot_be_read_logs_the_cause_and_a_fix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("plain-file");
        std::fs::write(&file, "x").expect("write a test file");
        // A file used as a folder makes `symlink_metadata` fail with a non-NotFound error.
        let under_a_file = file.join("child");
        let logs = capture_logs(|| restore_dir_write_bits(&under_a_file));
        assert!(
            logs.contains("cannot read"),
            "the log must name the failure: {logs}"
        );
        assert!(logs.contains("Fix:"), "the log must name a fix: {logs}");
    }

    #[test]
    fn a_missing_folder_has_nothing_to_restore_and_logs_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let logs = capture_logs(|| restore_dir_write_bits(&dir.path().join("absent")));
        assert!(logs.is_empty(), "a missing folder must stay silent: {logs}");
    }

    #[test]
    fn a_read_only_folder_gets_its_owner_bits_back() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("read-only");
        std::fs::create_dir(&folder).expect("create a test folder");
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555))
            .expect("make the folder read-only");
        restore_dir_write_bits(&folder);
        let mode = std::fs::metadata(&folder)
            .expect("stat the folder")
            .permissions()
            .mode();
        // Restore the mode so the temp dir can be removed.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755))
            .expect("restore the folder mode");
        assert_eq!(
            mode & 0o700,
            0o700,
            "the owner bits must come back: {mode:o}"
        );
    }

    #[test]
    fn a_link_is_never_chmod_and_its_target_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("create a test folder");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o555))
            .expect("make the target read-only");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create a test link");
        let logs = capture_logs(|| restore_dir_write_bits(&link));
        let mode = std::fs::metadata(&target)
            .expect("stat the target")
            .permissions()
            .mode();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("restore the target mode");
        assert_eq!(
            mode & 0o777,
            0o555,
            "a link target must keep its mode: {mode:o}"
        );
        assert!(logs.is_empty(), "a link is skipped silently: {logs}");
    }

    #[test]
    fn a_read_only_folder_inside_a_read_only_folder_is_restored_too() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).expect("create nested folders");
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o555))
            .expect("make the inner folder read-only");
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o555))
            .expect("make the outer folder read-only");
        restore_dir_write_bits(&outer);
        let outer_mode = std::fs::metadata(&outer)
            .expect("stat outer")
            .permissions()
            .mode();
        let inner_mode = std::fs::metadata(&inner)
            .expect("stat inner")
            .permissions()
            .mode();
        // Restore the modes so the temp dir can be removed.
        for folder in [&inner, &outer] {
            std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o755))
                .expect("restore the folder mode");
        }
        assert_eq!(outer_mode & 0o700, 0o700, "outer: {outer_mode:o}");
        assert_eq!(inner_mode & 0o700, 0o700, "inner: {inner_mode:o}");
    }
}
