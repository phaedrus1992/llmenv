//! Notices of a detached child, kept in files until a hook run reads them (#3398).
//!
//! A detached child runs in its own process, so its notice list dies with it. The child turns
//! the spool on at start, and each notice it records is also written to a file in the state
//! folder. The next hook run of any session reads the files into its own list and removes them,
//! so the agent sees the notice on that hook's output.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The folder under the state folder that holds the spooled notices.
const DIR: &str = "notice_spool";

/// At most this many spooled notices are read in one hook run, so a flood of child notices
/// cannot slow one hook. The rest wait for the next run.
const MAX_READ: usize = 32;

/// At most this many folder entries are listed in one hook run, so a folder that grew without
/// bound cannot slow one hook either.
const MAX_LIST: usize = 1024;

/// The most bytes of one spooled notice that a hook run reads. A notice is one capped line.
const MAX_NOTICE_BYTES: u64 = 64 * 1024;

static SPOOL: OnceLock<PathBuf> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Spool every later notice of this process under `state_dir`. Call it once, at the start of a
/// detached child; a second call keeps the first folder.
pub fn start(state_dir: &Path) {
    if SPOOL.set(state_dir.join(DIR)).is_err() {
        tracing::debug!("notice spool already started; keeping the first folder");
    }
}

/// Whether this process spools its notices, which is true in a detached child.
pub fn active() -> bool {
    SPOOL.get().is_some()
}

/// Write the cleaned `notice` to the spool, when this process spools. A failure is logged to the
/// child's log: the same unwritable state folder also fails the parent's own state writes, and
/// those report it to the agent.
pub(super) fn write(notice: &str) {
    let Some(dir) = SPOOL.get() else {
        return;
    };
    if let Err(e) = write_in(dir, notice) {
        tracing::error!(
            "cannot keep a notice for the agent in {}: {e}. Fix: check that the state folder is \
             writable. The notice was: {notice}",
            dir.display()
        );
    }
}

/// The file name sorts by write time first, so notices are read oldest first. The process id and
/// a sequence number keep two writes in one nanosecond apart.
fn write_in(dir: &Path, notice: &str) -> std::io::Result<()> {
    llmenv_paths::create_dir_owner_only(dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("{nanos:020}-{}-{seq}.txt", std::process::id());
    llmenv_paths::write_owner_only_atomic(&dir.join(name), notice.as_bytes())
}

/// Read the spooled notices under `state_dir` into this process's list, oldest first, and remove
/// each file that was read. An entry that cannot be listed, read, or removed becomes a notice
/// itself, so a broken spool is not silent.
pub fn collect(state_dir: &Path) {
    let dir = state_dir.join(DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            super::record_logged(format!(
                "llmenv: cannot read the notices of detached jobs in {}: {e}. Fix: check that the \
                 state folder is readable.",
                dir.display()
            ));
            return;
        }
    };
    let mut paths = Vec::new();
    for entry in entries.take(MAX_LIST) {
        match entry {
            Ok(entry) if entry.path().extension().is_some_and(|x| x == "txt") => {
                paths.push(entry.path());
            }
            Ok(_) => {}
            Err(e) => super::record_logged(format!(
                "llmenv: cannot list an entry of the detached job notices in {}: {e}. Fix: check \
                 that the state folder is readable.",
                dir.display()
            )),
        }
    }
    paths.sort();
    for path in paths.into_iter().take(MAX_READ) {
        collect_one(&path);
    }
}

fn collect_one(path: &Path) {
    // A link or another non-file is never read, so a planted link cannot pull in another file.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            match llmenv_paths::read_text_bounded(path, MAX_NOTICE_BYTES) {
                Ok(Some(text)) => super::record(text),
                Ok(None) => return,
                Err(e) => super::record_logged(format!(
                    "llmenv: cannot read the detached job notice {}: {e}. Fix: remove the file.",
                    path.display()
                )),
            }
        }
        // Another hook run read it first.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Ok(_) => super::record_logged(format!(
            "llmenv: the detached job notice {} is not a regular file, so it is removed unread.",
            path.display()
        )),
        Err(e) => super::record_logged(format!(
            "llmenv: cannot read the detached job notice {}: {e}. Fix: remove the file.",
            path.display()
        )),
    }
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        super::record_logged(format!(
            "llmenv: cannot remove the detached job notice {}: {e}. Fix: remove the file.",
            path.display()
        ));
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn notices() -> String {
        super::super::current().unwrap_or_default()
    }

    #[test]
    fn a_missing_spool_folder_is_no_notice() {
        let dir = tempfile::tempdir().unwrap();
        collect(dir.path());
        assert!(
            !notices().contains(&dir.path().display().to_string()),
            "{}",
            notices()
        );
    }

    #[test]
    fn an_unreadable_spool_file_is_a_notice_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join(DIR);
        std::fs::create_dir_all(spool.join("broken-spool-entry.txt")).unwrap();
        collect(dir.path());
        assert!(
            notices().contains("broken-spool-entry.txt"),
            "{}",
            notices()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_spool_is_removed_and_its_target_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join(DIR);
        std::fs::create_dir_all(&spool).unwrap();
        let target = dir.path().join("secret.txt");
        std::fs::write(&target, "spool link test: secret text").unwrap();
        let link = spool.join("0-planted-link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        collect(dir.path());
        assert!(
            !notices().contains("spool link test: secret text"),
            "{}",
            notices()
        );
        assert!(notices().contains("0-planted-link.txt"), "{}", notices());
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the link is removed"
        );
        assert!(target.exists(), "the target stays");
    }

    #[test]
    fn notices_are_read_oldest_first_and_at_most_the_cap_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join(DIR);
        for i in 0..MAX_READ + 3 {
            write_in(&spool, &format!("spool order test {i:03}")).unwrap();
        }
        collect(dir.path());
        let text = notices();
        assert!(text.contains("spool order test 000"), "{text}");
        assert!(
            !text.contains(&format!("spool order test {:03}", MAX_READ)),
            "{text}"
        );
        assert_eq!(
            std::fs::read_dir(&spool).unwrap().count(),
            3,
            "the rest wait"
        );
        collect(dir.path());
        assert!(notices().contains(&format!("spool order test {:03}", MAX_READ + 2)));
    }

    proptest! {
        /// Any notice written to the spool is read back once, and its file is removed.
        #[test]
        fn any_spooled_notice_is_read_back_and_removed(body in "[a-zA-Z0-9 .:]{1,60}") {
            let dir = tempfile::tempdir().unwrap();
            let marker = format!("spool round trip {}: {body}", dir.path().display());
            write_in(&dir.path().join(DIR), &marker).unwrap();
            collect(dir.path());
            prop_assert!(notices().contains(marker.trim_end()), "{}", notices());
            prop_assert_eq!(std::fs::read_dir(dir.path().join(DIR)).unwrap().count(), 0);
        }
    }
}
