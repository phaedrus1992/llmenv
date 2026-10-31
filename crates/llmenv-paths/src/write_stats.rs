//! Process-wide count of files the shared owner-only writers have written.
//!
//! The writers also skip a file that already holds the same bytes at the same mode. A skip is
//! counted apart from a write, so a repeat run shows `files == 0` (#3020).
//!
//! A caller that wants "how many files did this step write" takes a [`snapshot`] before the step
//! and calls [`WriteStats::since`] after it (#3020).

use std::sync::atomic::{AtomicU64, Ordering};

static FILES: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// A count of files written and the bytes they held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteStats {
    /// Files written.
    pub files: u64,
    /// Bytes written.
    pub bytes: u64,
    /// Files left alone because they already held the same bytes at the same mode.
    pub skipped: u64,
}

impl WriteStats {
    /// The writes made after `earlier` was taken.
    #[must_use]
    pub fn since(self, earlier: WriteStats) -> WriteStats {
        WriteStats {
            files: self.files.saturating_sub(earlier.files),
            bytes: self.bytes.saturating_sub(earlier.bytes),
            skipped: self.skipped.saturating_sub(earlier.skipped),
        }
    }
}

/// Record one finished write of `bytes` bytes.
pub(crate) fn note_write(bytes: usize) {
    FILES.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// Whether `path` is a regular file that holds `content` at permission bits `mode`.
///
/// A symlink, a missing file, a different size, a different mode, or an unreadable file all give
/// `false`, and the caller then writes as usual. A `true` is recorded as a skip, and the file's
/// modified time is set to now.
pub(crate) fn already_current(path: &std::path::Path, content: &[u8], mode: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return false;
        };
        let same_shape = meta.is_file()
            && meta.permissions().mode() & 0o777 == mode
            && usize::try_from(meta.len()).is_ok_and(|len| len == content.len());
        if same_shape && std::fs::read(path).is_ok_and(|old| old == content) && touch(path) {
            SKIPPED.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = (path, content, mode);
        false
    }
}

/// Mark `path` as modified now. A skipped write still counts as a write for a reader that prunes
/// by age, such as the recall ledger. Returns `false` when the time cannot be set, so the caller
/// then writes the file as usual.
#[cfg(unix)]
fn touch(path: &std::path::Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt as _;
    // A link at `path` is refused, so the mtime of a link's target is never changed (#3139).
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .inspect_err(|e| tracing::debug!("cannot touch {}, so the write runs: {e}", path.display()))
        .is_ok()
}

/// The writes made by this process so far.
#[must_use]
pub fn snapshot() -> WriteStats {
    WriteStats {
        files: FILES.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        skipped: SKIPPED.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn since_counts_only_the_writes_after_the_snapshot() {
        let before = snapshot();
        note_write(5);
        note_write(7);
        let delta = snapshot().since(before);
        assert!(delta.files >= 2 && delta.bytes >= 12);
    }

    // A skipped write still marks the file as modified, so an age-based prune keeps it.
    #[cfg(unix)]
    #[test]
    fn a_skipped_write_refreshes_the_modified_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"abc").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(already_current(&path, b"abc", 0o600));
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after > old + std::time::Duration::from_secs(3600));
    }

    #[cfg(unix)]
    #[test]
    fn already_current_needs_same_bytes_and_same_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        assert!(
            !already_current(&path, b"abc", 0o600),
            "a missing file is not current"
        );
        std::fs::write(&path, b"abc").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(already_current(&path, b"abc", 0o600));
        assert!(!already_current(&path, b"abd", 0o600), "other bytes");
        assert!(!already_current(&path, b"abcd", 0o600), "other size");
        assert!(!already_current(&path, b"abc", 0o700), "other mode");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !already_current(&path, b"abc", 0o600),
            "a loose file is rewritten"
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            !already_current(&link, b"abc", 0o644),
            "a symlink is never current"
        );
    }

    proptest::proptest! {
        // `since` never panics and equals a saturating difference, even when `earlier` is larger.
        #[test]
        fn since_is_a_saturating_difference(
            a in (0u64..u64::MAX, 0u64..u64::MAX, 0u64..u64::MAX),
            b in (0u64..u64::MAX, 0u64..u64::MAX, 0u64..u64::MAX),
        ) {
            let now = WriteStats { files: a.0, bytes: a.1, skipped: a.2 };
            let before = WriteStats { files: b.0, bytes: b.1, skipped: b.2 };
            let delta = now.since(before);
            proptest::prop_assert_eq!(delta.files, a.0.saturating_sub(b.0));
            proptest::prop_assert_eq!(delta.bytes, a.1.saturating_sub(b.1));
            proptest::prop_assert_eq!(delta.skipped, a.2.saturating_sub(b.2));
            proptest::prop_assert_eq!(now.since(now), WriteStats::default());
        }

        // A file that holds the bytes at the mode is current. Other bytes or another mode are not.
        #[cfg(unix)]
        #[test]
        fn already_current_is_true_only_for_the_same_bytes_and_mode(
            content in proptest::collection::vec(proptest::num::u8::ANY, 0..64),
            other in proptest::collection::vec(proptest::num::u8::ANY, 0..64),
            mode in 0o600u32..=0o777,
            other_mode in 0o600u32..=0o777,
        ) {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("f");
            std::fs::write(&path, &content).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            proptest::prop_assert!(already_current(&path, &content, mode));
            proptest::prop_assert_eq!(
                already_current(&path, &other, mode),
                other == content,
            );
            proptest::prop_assert_eq!(
                already_current(&path, &content, other_mode),
                other_mode == mode,
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn touch_never_changes_the_mtime_of_a_link_target() {
        use std::time::{Duration, UNIX_EPOCH};
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target.json");
        std::fs::write(&target, b"{}").expect("write the target");
        let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&target)
            .expect("open the target")
            .set_modified(old)
            .expect("age the target");
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).expect("plant a link");
        assert!(
            !touch(&link),
            "a link must be refused, so touch reports no change"
        );
        let mtime = std::fs::metadata(&target)
            .expect("stat the target")
            .modified();
        assert_eq!(mtime.ok(), Some(old), "the target's mtime must not move");
    }
}
