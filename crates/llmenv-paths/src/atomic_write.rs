//! Atomic owner-only writes, and writes that keep a user's symlink in place.

use std::path::{Path, PathBuf};

use crate::create_dir_owner_only;

/// Atomically write `content` to `path` with owner-only permissions.
///
/// Steps: write to a same-directory temp file `<path>.<pid>.<nanos>.tmp`,
/// `fsync` it for durability, then `rename` over the destination (POSIX
/// atomic replace). Readers observing `path` mid-write see either the prior
/// good contents or the new contents — never a torn document. On error the
/// temp file is removed.
///
/// Use for any structured/JSON state file where a half-written file would
/// break the next read: `icm.json`, `sync.json`, `settings.json`, `mcp.json`.
pub fn write_owner_only_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    write_atomic(path, content, true)
}

/// The shared write. `sync` says whether the temp file is flushed to the disk before the rename.
pub(crate) fn write_atomic(path: &Path, content: &[u8], sync: bool) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", path.display()),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", path.display()),
        )
    })?;
    // A bare name such as "foo.json" has an empty parent. Writing it into the working directory
    // would land the file in the user's project, so the caller must pass a full path.
    if parent.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "path has no directory component: {}. Pass a path that includes its directory",
                path.display()
            ),
        ));
    }
    // Born owner-only (0o700) at creation, including every missing
    // ancestor — a create_dir_all + post-hoc set_permissions leaves the
    // umask default (typically 0o755, world-listable) both as a TOCTOU
    // window on the immediate parent and permanently on any intermediate
    // ancestor it doesn't chmod (#1178).
    create_dir_owner_only(parent).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("creating/hardening directory {}: {e}", parent.display()),
        )
    })?;
    write_owner_only_atomic_in_dir(parent, file_name, path, content, sync)
}

/// Process-local counter used to disambiguate temp filenames when multiple
/// calls within the same process land in the same nanosecond. Combined with
/// `pid` and `nanos`, this guarantees uniqueness within a process and is
/// extremely unlikely to collide across processes (different pids).
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_owner_only_atomic_in_dir(
    parent: &Path,
    file_name: &std::ffi::OsStr,
    final_path: &Path,
    content: &[u8],
    sync: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    // The atomic write always leaves mode 0o600, so a file that already holds these bytes at 0o600
    // needs no rewrite.
    if crate::write_stats::already_current(final_path, content, 0o600) {
        return Ok(());
    }
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    // Retry on EEXIST up to a small number of times. A stale temp file (from
    // a prior crashed process with the same pid+nanos slice) or in-process
    // race could collide; the per-process counter and retry loop together
    // guarantee progress without unbounded blocking.
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..8 {
        let counter = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".{pid}.{nanos}.{counter}.tmp"));
        let tmp_path = parent.join(&tmp_name);

        let result = (|| -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp_path)?;
            file.write_all(content)?;
            if sync {
                file.sync_all()?;
            }
            std::fs::rename(&tmp_path, final_path)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                crate::write_stats::note_write(content.len());
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                // This crate has no logger, so a failed cleanup is carried in the returned error.
                return match std::fs::remove_file(&tmp_path) {
                    Err(cleanup) if cleanup.kind() != std::io::ErrorKind::NotFound => {
                        Err(std::io::Error::new(
                            e.kind(),
                            format!(
                                "{e}; also could not remove temp file {}: {cleanup}",
                                tmp_path.display()
                            ),
                        ))
                    }
                    _ => Err(e),
                };
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temp-file collision retries",
        )
    }))
}

/// The target of a symlink at `path`, or `None` when `path` is not a symlink or does not exist.
fn user_link_target(path: &Path) -> std::io::Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(path).map(Some).map_err(|e| {
                let target = std::fs::read_link(path).map_or_else(
                    |_| "a target that cannot be read".to_string(),
                    |t| t.display().to_string(),
                );
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "cannot follow the symlink {} to {target}: {e}. Create the file that it \
                         points to, or remove the link so that llmenv writes a plain file",
                        path.display()
                    ),
                )
            })
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Return the file that a symlink at `path` points to, or `path` itself when it is not a symlink
/// or does not exist yet.
///
/// Use this only for a file that the user can keep in a dotfiles tree, such as an agent config
/// file. A path under a tree that llmenv owns, or a state file, must refuse a symlink instead
/// (#1341, #2599, #2686).
///
/// # Errors
/// `path` is a symlink whose target cannot be resolved, such as a dangling link. The error names
/// the link, its target, and the fix.
pub fn resolve_user_link(path: &Path) -> std::io::Result<PathBuf> {
    Ok(user_link_target(path)?.unwrap_or_else(|| path.to_path_buf()))
}

/// Write a file that the user can keep as a symlink, such as a dotfiles-managed agent config file.
///
/// A symlink at `path` stays in place (#2883). The content goes to the link target, and the
/// atomic write happens in the target's own directory. That directory belongs to the user, so
/// this write does not create it and does not change its mode. A path that is not a symlink gets
/// [`write_owner_only_atomic`]. In both cases the written file is owner-only.
///
/// The same policy limit as [`resolve_user_link`] applies.
///
/// # Errors
/// The link target cannot be resolved, or the write fails.
pub fn write_user_file_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let Some(target) = user_link_target(path)? else {
        return write_owner_only_atomic(path, content);
    };
    let (Some(parent), Some(file_name)) = (target.parent(), target.file_name()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "the symlink {} points to {}, which is not a file. Point the link at a file",
                path.display(),
                target.display()
            ),
        ));
    };
    write_owner_only_atomic_in_dir(parent, file_name, &target, content, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn link_to_file(dir: &Path, content: &[u8]) -> (PathBuf, PathBuf) {
        let target = dir.join("target");
        std::fs::write(&target, content).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        (target, link)
    }

    #[cfg(unix)]
    #[test]
    fn resolve_user_link_returns_the_target_of_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let (target, link) = link_to_file(dir.path(), b"");
        let resolved = resolve_user_link(&link).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&target).unwrap());
    }

    #[test]
    fn resolve_user_link_returns_a_plain_or_missing_path_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        let missing = dir.path().join("missing");
        assert_eq!(resolve_user_link(&plain).unwrap(), plain);
        assert_eq!(resolve_user_link(&missing).unwrap(), missing);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_user_link_names_the_link_and_the_fix_for_a_dangling_link() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("gone"), &link).unwrap();
        let err = resolve_user_link(&link).unwrap_err().to_string();
        assert!(err.contains(&link.display().to_string()), "{err}");
        assert!(
            err.contains("gone"),
            "the error must name the link target: {err}"
        );
        assert!(
            err.contains("remove the link"),
            "the error must give the fix: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_user_file_atomic_keeps_the_link_and_writes_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let (target, link) = link_to_file(dir.path(), b"old");
        write_user_file_atomic(&link, b"new").unwrap();
        assert!(link.is_symlink(), "the write must not replace the link");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn write_user_file_atomic_leaves_the_target_directory_mode_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_, link) = link_to_file(&dotfiles, b"old");
        let config_link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&link, &config_link).unwrap();
        write_user_file_atomic(&config_link, b"new").unwrap();
        let mode = std::fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the user's directory must keep its mode");
    }

    #[test]
    fn write_user_file_atomic_writes_a_plain_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.yaml");
        write_user_file_atomic(&path, b"key: value").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"key: value");
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_atomic_creates_file_with_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("atomic");
        write_owner_only_atomic(&path, b"payload").expect("atomic write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        assert_eq!(std::fs::read(&path).expect("read"), b"payload");
    }

    #[test]
    fn write_owner_only_atomic_replaces_existing_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("file");
        write_owner_only_atomic(&path, b"v1").expect("v1");
        write_owner_only_atomic(&path, b"v2-longer").expect("v2");
        assert_eq!(std::fs::read(&path).expect("read"), b"v2-longer");
    }

    #[test]
    fn write_owner_only_atomic_leaves_no_temp_files() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("file");
        write_owner_only_atomic(&path, b"x").expect("write");
        write_owner_only_atomic(&path, b"y").expect("write");
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "found stray files: {entries:?}");
    }

    #[test]
    fn write_owner_only_atomic_creates_parent_dir() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("a/b/c/file.json");
        write_owner_only_atomic(&path, b"nested").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"nested");
    }

    // #1178: write_owner_only_atomic must create every missing ancestor
    // directory owner-only from the moment of creation, not just the
    // immediate parent. The old create_dir_all + post-hoc set_permissions
    // approach only chmods the immediate parent, leaving intermediate
    // ancestors at the umask default (world-readable) forever, and leaves a
    // TOCTOU window on the immediate parent between creation and chmod.
    #[cfg(unix)]
    #[test]
    fn write_owner_only_atomic_creates_all_missing_ancestors_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp
            .path()
            .join("grandparent")
            .join("parent")
            .join("file.json");
        write_owner_only_atomic(&path, b"nested").expect("write");

        for ancestor in ["grandparent", "grandparent/parent"] {
            let dir = tmp.path().join(ancestor);
            let mode = std::fs::metadata(&dir)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "{ancestor} must be born owner-only, got {mode:o}"
            );
        }
    }

    #[test]
    fn write_owner_only_atomic_concurrent_writers_no_torn_reads() {
        // Spawn N threads writing distinct fixed-size payloads to the same
        // path. Every reader sees one of the written payloads — never a
        // partial document, never an empty file.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("contended.json");
        write_owner_only_atomic(&path, b"initial").expect("seed");

        let payloads: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("{{\"writer\":{i},\"data\":\"{}\"}}", "x".repeat(256)).into_bytes())
            .collect();
        let valid: std::collections::HashSet<Vec<u8>> = std::iter::once(b"initial".to_vec())
            .chain(payloads.iter().cloned())
            .collect();

        let writers: Vec<_> = payloads
            .into_iter()
            .map(|payload| {
                let p = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        write_owner_only_atomic(&p, &payload).expect("concurrent write");
                    }
                })
            })
            .collect();

        let reader_path = path.clone();
        let reader_valid = valid.clone();
        let reader = std::thread::spawn(move || {
            for _ in 0..200 {
                let body = std::fs::read(&reader_path).expect("concurrent read");
                assert!(
                    reader_valid.contains(&body),
                    "reader observed torn write: {body:?}"
                );
            }
        });

        for w in writers {
            w.join().expect("writer join");
        }
        reader.join().expect("reader join");
    }

    #[test]
    fn write_owner_only_atomic_refuses_a_bare_file_name() {
        let err = write_owner_only_atomic(Path::new("bare.json"), b"x")
            .expect_err("a bare name must not land in the working directory");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
