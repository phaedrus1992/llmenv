//! Notices of a detached child, kept in files until a hook run reads them (#3398).
//!
//! A detached child runs in its own process, so its notice list dies with it. The child turns
//! the spool on at start, and each notice it records is also written to a file in the state
//! folder. The next hook run reads the files into its own list and removes them, so the agent
//! sees the notice on its next hook output.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The folder under the state folder that holds the spooled notices.
const DIR: &str = "notice_spool";

/// At most this many spooled notices are read in one hook run, so a flood of child notices
/// cannot slow one hook. The rest wait for the next run.
const MAX_READ: usize = 32;

static SPOOL: OnceLock<PathBuf> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Spool every later notice of this process under `state_dir`. Call it once, at the start of a
/// detached child.
pub(crate) fn start(state_dir: &Path) {
    // A second call in one process keeps the first folder; there is one state folder per run.
    let _ = SPOOL.set(state_dir.join(DIR));
}

/// Write `notice` to the spool, when this process spools. A failure is logged: the child has no
/// other channel to the agent.
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

fn write_in(dir: &Path, notice: &str) -> std::io::Result<()> {
    crate::paths::create_dir_owner_only(dir)?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}-{seq}.txt", std::process::id());
    crate::paths::write_owner_only_atomic(&dir.join(name), notice.as_bytes())
}

/// Read the spooled notices under `state_dir` into this process's list, and remove each file
/// that was read. A file that cannot be read or removed becomes a notice itself, so a broken
/// spool is not silent.
pub(crate) fn collect(state_dir: &Path) {
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
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    paths.sort();
    for path in paths.into_iter().take(MAX_READ) {
        collect_one(&path);
    }
}

fn collect_one(path: &Path) {
    match crate::paths::bounded::state_text_io(path) {
        Ok(text) => super::record(text),
        // Another hook run read it first.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
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

    #[test]
    fn a_spooled_notice_reaches_the_list_of_the_next_run_once() {
        let dir = tempfile::tempdir().unwrap();
        write_in(
            &dir.path().join(DIR),
            "spool test: the index result was not saved",
        )
        .unwrap();
        collect(dir.path());
        let notices = super::super::current().unwrap_or_default();
        assert!(
            notices.contains("spool test: the index result was not saved"),
            "{notices}"
        );
        let left = std::fs::read_dir(dir.path().join(DIR)).unwrap().count();
        assert_eq!(left, 0, "a read notice is removed");
    }

    #[test]
    fn a_missing_spool_folder_is_no_notice() {
        let dir = tempfile::tempdir().unwrap();
        collect(dir.path());
        let notices = super::super::current().unwrap_or_default();
        assert!(!notices.contains("detached job"), "{notices}");
    }

    #[test]
    fn an_unreadable_spool_file_is_a_notice_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join(DIR);
        std::fs::create_dir_all(spool.join("broken-spool-entry.txt")).unwrap();
        collect(dir.path());
        let notices = super::super::current().unwrap_or_default();
        assert!(notices.contains("broken-spool-entry.txt"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    #[test]
    fn at_most_the_read_cap_is_collected_in_one_run() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join(DIR);
        for i in 0..MAX_READ + 3 {
            write_in(&spool, &format!("spool cap test {i}")).unwrap();
        }
        collect(dir.path());
        let left = std::fs::read_dir(&spool).unwrap().count();
        assert_eq!(left, 3, "the rest wait for the next run");
    }
}
