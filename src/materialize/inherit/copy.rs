//! Copies one state file between the durable store and a config folder (#3139).

use std::path::Path;

/// Mode of every copied file: owner read and write only.
const OWNER_ONLY: u32 = 0o600;

/// Copy the regular file `src` to `dst`. Does nothing when `dst` already exists.
///
/// The source opens with a link refusal and `O_NONBLOCK`, so a named pipe swapped in for it cannot
/// block the open, and the open file must be regular. The destination opens with `create_new`,
/// which refuses an existing path and a dangling link. It is created owner-only, whatever the
/// source mode, because every file this module copies is a credential or prompt history (#1421).
/// A failed copy removes the partial file, because the next pass would otherwise skip the damaged
/// file as present.
///
/// `dirfd::copy_file_at` is not used here: it unlinks an existing destination first, which would
/// delete a file that appears between the caller's check and this copy.
pub(super) fn copy_file_no_follow(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    // One flag set: `custom_flags` replaces earlier flags, so a separate call for NOFOLLOW would
    // drop NONBLOCK and let a fifo block the open.
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits().cast_signed())
        .open(src)?;
    llmenv_paths::check_regular_file(&input)?;
    let mut dst_opts = std::fs::OpenOptions::new();
    dst_opts.write(true).create_new(true).mode(OWNER_ONLY);
    let mut output = match dst_opts.open(dst) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    };
    // `mode` on open is masked by the umask, so the mode is set again to reach the full floor.
    let copied = std::io::copy(&mut input, &mut output)
        .and_then(|_| {
            output.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(OWNER_ONLY))
        })
        .map(|_| ());
    if copied.is_err() {
        drop(output);
        if let Err(e) = std::fs::remove_file(dst) {
            tracing::error!(
                path = %dst.display(),
                error = %e,
                "inherit: could not remove a partial copy. Fix: remove the file by hand."
            );
        }
    }
    copied
}

/// True when `path` is a regular file (`symlink_metadata`, not `is_file()`
/// — #1341: `is_file()` follows a symlink, so a symlinked entry under
/// `state_dir`/`config_dir` would otherwise be read/copied through
/// silently). Skips (warns) rather than errors: a stray symlink here
/// shouldn't block a whole `materialize`/inherit pass over one file. A
/// missing source is the documented, common no-op case (nothing cached
/// yet) and stays silent; any other stat failure (permission denied, ...)
/// is warned rather than folded into the same "absent" outcome
/// (security-audit, #1341).
pub(super) fn is_real_file(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // The skip is intended (#1341), so it runs on every pass and is debug only.
            tracing::debug!(path = %path.display(), "inherit: skipping symlinked file");
            false
        }
        Ok(meta) => meta.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "inherit: could not stat file, skipping");
            false
        }
    }
}

/// True when something already exists at `path`, including a dangling
/// symlink (`symlink_metadata`, not `exists()` — #1341 security-audit:
/// `exists()` follows a symlink and reports `false` for a dangling one,
/// which would fall through to a copy opening
/// `O_CREAT|O_TRUNC` through the link and writing the copied content at
/// the link's target instead of replacing the link itself).
pub(super) fn dest_already_present(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                tracing::warn!(
                    path = %path.display(),
                    "inherit: skipping copy — destination is a symlink"
                );
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        // The destination state is unknown, so the copy is skipped. A copy here could write
        // through a link the check could not see (#1341), so the skip is the safe result.
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "inherit: skipping copy — cannot read the destination. Fix: check the \
                 destination's permissions, then run the command again."
            );
            true
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "a test: a failed setup must stop it")]
mod tests {
    use super::*;

    #[test]
    fn a_link_at_the_destination_is_not_written_through() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.jsonl");
        std::fs::write(&src, b"new\n").expect("write the source");
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"keep me").expect("write the victim");
        let dst = dir.path().join("dst.jsonl");
        std::os::unix::fs::symlink(&victim, &dst).expect("plant a link at the destination");
        copy_file_no_follow(&src, &dst).expect("an existing destination is a skip");
        assert_eq!(std::fs::read(&victim).expect("read the victim"), b"keep me");
    }

    #[test]
    fn a_link_as_the_source_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let real = dir.path().join("real.jsonl");
        std::fs::write(&real, b"secret\n").expect("write the real file");
        let src = dir.path().join("src.jsonl");
        std::os::unix::fs::symlink(&real, &src).expect("plant a link as the source");
        let dst = dir.path().join("dst.jsonl");
        assert!(
            copy_file_no_follow(&src, &dst).is_err(),
            "a linked source must be refused"
        );
        assert!(!dst.exists(), "nothing may be copied from a link");
    }

    #[test]
    fn a_regular_file_is_copied_with_its_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.jsonl");
        std::fs::write(&src, b"line\n").expect("write the source");
        let dst = dir.path().join("dst.jsonl");
        copy_file_no_follow(&src, &dst).expect("copy a regular file");
        assert_eq!(std::fs::read(&dst).expect("read the copy"), b"line\n");
    }

    #[test]
    fn a_named_pipe_as_the_source_is_refused_without_blocking() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("pipe.jsonl");
        let made = std::process::Command::new("mkfifo")
            .arg(&src)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo must make the test pipe");
        let dst = dir.path().join("dst.jsonl");
        assert!(
            copy_file_no_follow(&src, &dst).is_err(),
            "a fifo must be refused"
        );
        assert!(!dst.exists(), "nothing may be copied from a fifo");
    }

    #[test]
    fn the_copy_is_owner_only_whatever_the_source_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.jsonl");
        std::fs::write(&src, b"line\n").expect("write the source");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644))
            .expect("set the source mode");
        let dst = dir.path().join("dst.jsonl");
        copy_file_no_follow(&src, &dst).expect("copy a regular file");
        let mode = std::fs::metadata(&dst)
            .expect("stat the copy")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the copy must be owner-only: {mode:o}");
    }
}
