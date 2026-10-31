//! The proxy spawn lock: take it, record the holder's pid, and release it.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Takes the spawn lock, recording the holder's pid in it.
///
/// Returns `Ok(None)` when someone else already holds it. The pid is recorded so
/// a lock left behind by an export that died mid-spawn can be recognized as
/// stale — see [`super::adopt_or_reclaim`].
///
/// The pid goes into a staging file first, which is then hard-linked to the lock
/// path. The lock therefore never exists empty: a peer that reads an empty lock
/// would treat it as stale and reclaim a live holder's lock (#2587).
///
/// # Errors
/// Returns an error if the staging file cannot be written or linked for a reason
/// other than the lock already existing. The state directory must support hard
/// links.
pub(super) fn try_lock(lock_path: &Path) -> anyhow::Result<Option<()>> {
    let staging = staging_path(lock_path);
    write_staging(&staging)?;
    let linked = std::fs::hard_link(&staging, lock_path);
    if let Err(e) = std::fs::remove_file(&staging) {
        // A leftover staging file is never read again, but it stays on disk. Hook stderr
        // never reaches the agent, so the leftover is also a notice (#3204).
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: could not remove proxy lock staging file {} ({e}). Fix: remove the file \
             by hand.",
            staging.display()
        ));
    }
    match linked {
        Ok(()) => Ok(Some(())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "creating proxy lockfile {} (the state directory must support hard links)",
            lock_path.display()
        ))),
    }
}

/// The staging file for this process: `<lock>.<pid>.staging`. The suffix differs
/// from the pidfile temp name, so the two never share a file.
pub(super) fn staging_path(lock_path: &Path) -> PathBuf {
    let mut name = lock_path.as_os_str().to_owned();
    name.push(format!(".{}.staging", std::process::id()));
    PathBuf::from(name)
}

/// Writes this process's pid to a new owner-only staging file. A leftover file
/// of a crashed export with the same pid is removed first. `create_new` refuses
/// a symlink planted at the name, so the write never follows it.
fn write_staging(staging: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    match std::fs::remove_file(staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "removing old proxy lock staging file {}",
                staging.display()
            )));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(staging)
        .with_context(|| format!("creating proxy lock staging file {}", staging.display()))?;
    file.write_all(std::process::id().to_string().as_bytes())
        .with_context(|| format!("writing proxy lock staging file {}", staging.display()))
}

/// Releases the spawn lock, complaining loudly if it can't.
///
/// A lock that won't release makes every later export fail, so unlike most
/// cleanup here this is not silently discarded.
pub(super) fn release_lock(lock_path: &Path) {
    match std::fs::remove_file(lock_path) {
        Ok(()) => {}
        // Already gone: the lock is released, which is the goal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // A kept lock blocks later proxy starts. Hook stderr never reaches the agent (#3204).
        Err(e) => llmenv_util::agent_notice::record_logged(format!(
            "llmenv: could not release proxy lockfile {} ({e}). Fix: remove it manually if \
             mcp-proxy stops starting.",
            lock_path.display()
        )),
    }
}

/// Reads the pid recorded in the lockfile, if it holds one.
///
/// An absent lock and a lock with no readable pid both give `Ok(None)`. `try_lock`
/// never leaves a lock without a pid, so one without it comes from an older
/// llmenv or a manual edit and reads as stale. Any other
/// read failure is an error. Reading it as "no holder" would let a peer reclaim
/// a lock whose holder may still be alive (#2587).
pub(super) fn lock_holder(lock_path: &Path) -> anyhow::Result<Option<u32>> {
    match std::fs::read_to_string(lock_path) {
        Ok(text) => Ok(text.trim().parse::<u32>().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("reading proxy lockfile {}", lock_path.display()))),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod lock_notice_tests {
    use super::{lock_holder, release_lock, write_staging};
    use proptest::prelude::*;

    /// #3204: a lockfile that cannot be removed is named for the agent. A directory at the
    /// lock path makes `remove_file` fail for a real reason.
    #[test]
    fn failed_lock_release_is_recorded_for_the_agent() {
        let dir = tempfile::tempdir().expect("test");
        let lock = dir.path().join("agent-notice-3204-stuck.lock");
        std::fs::create_dir(&lock).expect("test");
        release_lock(&lock);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains(&lock.display().to_string()),
            "a failed lock release must name the lockfile: {notices}"
        );
    }

    /// A lockfile that is removed normally adds no notice.
    #[test]
    fn released_lock_adds_no_notice() {
        let dir = tempfile::tempdir().expect("test");
        let lock = dir.path().join("agent-notice-3204-released.lock");
        std::fs::write(&lock, b"").expect("test");
        release_lock(&lock);
        assert!(!lock.exists());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(!notices.contains("agent-notice-3204-released"));
    }

    /// A lock that is already gone is released already, so it adds no notice.
    #[test]
    fn already_removed_lock_adds_no_notice() {
        let dir = tempfile::tempdir().expect("test");
        let lock = dir.path().join("agent-notice-3204-gone.lock");
        release_lock(&lock);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(!notices.contains("agent-notice-3204-gone"));
    }

    /// The staging writer records this process's pid, and `lock_holder` reads it back.
    #[test]
    fn staging_writer_and_reader_agree_on_the_pid() {
        let dir = tempfile::tempdir().expect("test");
        let lock = dir.path().join("proxy.lock");
        write_staging(&lock).expect("test");
        assert_eq!(lock_holder(&lock).expect("test"), Some(std::process::id()));
    }

    proptest! {
        /// Any u32 written as decimal text reads back as the same pid.
        #[test]
        fn any_pid_reads_back_unchanged(pid in any::<u32>()) {
            let dir = tempfile::tempdir().expect("test");
            let lock = dir.path().join("proxy.lock");
            std::fs::write(&lock, pid.to_string()).expect("test");
            prop_assert_eq!(lock_holder(&lock).expect("test"), Some(pid));
        }

        /// Text that is not a pid reads as no holder, never as a panic or an error.
        #[test]
        fn non_numeric_lock_reads_as_no_holder(text in "[a-z]{1,24}") {
            let dir = tempfile::tempdir().expect("test");
            let lock = dir.path().join("proxy.lock");
            std::fs::write(&lock, &text).expect("test");
            prop_assert_eq!(lock_holder(&lock).expect("test"), None);
        }
    }
}
