//! The proxy spawn lock: take it, record the holder's pid and start time, and release it.

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
    file.write_all(holder_text(std::process::id(), start_secs(std::process::id())).as_bytes())
        .with_context(|| format!("writing proxy lock staging file {}", staging.display()))
}

/// When the process `pid` started, in seconds since the epoch. `None` when `ps` cannot tell.
fn start_secs(pid: u32) -> Option<u64> {
    let start = llmenv_util::proc_start::process_start(pid)?;
    Some(start.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs())
}

/// The text of a lock: the pid, and the start time of that process when it is known.
fn holder_text(pid: u32, start: Option<u64>) -> String {
    start.map_or_else(|| pid.to_string(), |secs| format!("{pid} {secs}"))
}

/// Two reads of one start time can differ by this much. `ps` counts whole seconds. On Linux the
/// age comes from the time since boot, so a step of the wall clock moves the start as well. A
/// reused pid starts much later than this, because the holder lives for seconds and a reboot
/// takes longer.
const START_TOLERANCE_SECS: u64 = 30;

/// The process that took the spawn lock (#3795).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LockHolder {
    pub(super) pid: u32,
    /// When the holder started. `None` for a lock of an older llmenv, or a start that is not a
    /// full epoch time.
    start: Option<u64>,
}

impl LockHolder {
    /// Whether `pid` now names a process that started after the lock was taken. A pid is reused
    /// after its process exits, for example after a reboot, so the lock is stale then. A
    /// signal check cannot see that when the new process belongs to this user. `None` when `ps`
    /// cannot say. A lock with no recorded start has no sign of reuse.
    pub(super) fn pid_was_reused(&self) -> Option<bool> {
        let Some(recorded) = self.start else {
            return Some(false);
        };
        start_secs(self.pid).map(|current| start_is_later(recorded, current))
    }
}

/// Whether `current` is later than `recorded` by more than the clock tolerance.
fn start_is_later(recorded: u64, current: u64) -> bool {
    current > recorded.saturating_add(START_TOLERANCE_SECS)
}

/// A decimal number of ASCII digits, and nothing else. `str::parse` also takes a leading `+`.
fn digits(word: &str) -> Option<u64> {
    let ascii = !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    ascii.then(|| word.parse().ok()).flatten()
}

/// Reads `<pid>` or `<pid> <start>`. Anything else is not a lock of llmenv. A start that is not
/// 10 digits, such as one cut short, is dropped: the lock then has only its pid.
fn parse_holder(text: &str) -> Option<LockHolder> {
    let mut words = text.split_whitespace();
    // A pid is a positive i32 for `kill(2)`. Any other number is not a pid of this lock.
    let pid = u32::try_from(digits(words.next()?)?)
        .ok()
        .filter(|p| (1..=i32::MAX.cast_unsigned()).contains(p))?;
    let second = words.next();
    let start = second.and_then(|w| digits(w).filter(|_| w.len() == 10));
    if second.is_some() && start.is_none() {
        tracing::warn!(
            lock = %llmenv_util::strip_unsafe_chars(text),
            "the spawn lock has a start time that is not an epoch time"
        );
    }
    words.next().is_none().then_some(LockHolder { pid, start })
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

/// The most bytes of a pidfile or a lockfile that llmenv reads. Each holds one pid.
const MAX_PID_FILE_BYTES: u64 = 4 * 1024;

/// Read a pidfile or a lockfile. A FIFO, a link, or a file past the cap is an error, and a missing
/// file is `NotFound`, so a caller cannot take a planted file for an absent one (#3457).
///
/// # Errors
/// The file is missing, is not a regular file, is too large, or is not UTF-8.
pub(super) fn read_state_text(path: &Path) -> std::io::Result<String> {
    llmenv_paths::read_text_bounded(path, MAX_PID_FILE_BYTES)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is missing", path.display()),
        )
    })
}

/// Reads the pid recorded in the lockfile, if it holds one.
///
/// An absent lock and a lock with no readable pid both give `Ok(None)`. `try_lock`
/// never leaves a lock without a pid, so one without it comes from an older
/// llmenv or a manual edit and reads as stale. Any other
/// read failure is an error. Reading it as "no holder" would let a peer reclaim
/// a lock whose holder may still be alive (#2587).
pub(super) fn lock_holder(lock_path: &Path) -> anyhow::Result<Option<LockHolder>> {
    match read_state_text(lock_path) {
        Ok(text) => {
            let holder = parse_holder(&text);
            if holder.is_none() && !text.trim().is_empty() {
                tracing::warn!(
                    lock = %llmenv_util::strip_unsafe_chars(&text),
                    "the spawn lock is not a pid and a start time, so it reads as stale"
                );
            }
            Ok(holder)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("reading proxy lockfile {}", lock_path.display()))),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod lock_notice_tests {
    use super::{
        LockHolder, lock_holder, parse_holder, release_lock, start_is_later, write_staging,
    };
    use proptest::prelude::*;

    /// A dangling link at the lock path is an error, not "no holder": a peer must not reclaim a
    /// lock that it cannot read (#2587, #3457).
    #[cfg(unix)]
    #[test]
    fn a_dangling_link_at_the_lock_path_is_an_error() {
        let dir = tempfile::tempdir().expect("test");
        let lock = dir.path().join("proxy.lock");
        std::os::unix::fs::symlink(dir.path().join("gone"), &lock).expect("test");
        let err = lock_holder(&lock).expect_err("a dangling link is not an empty lock");
        assert!(format!("{err:#}").contains("link"), "{err:#}");
    }

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
        let holder = lock_holder(&lock).expect("test").expect("a holder");
        assert_eq!(holder.pid, std::process::id());
        // This process has not been replaced by another one with its pid.
        assert_ne!(holder.pid_was_reused(), Some(true));
    }

    proptest! {
        /// Any u32 written as decimal text reads back as the same pid.
        #[test]
        fn any_pid_reads_back_unchanged(pid in 1..=i32::MAX.cast_unsigned()) {
            let dir = tempfile::tempdir().expect("test");
            let lock = dir.path().join("proxy.lock");
            std::fs::write(&lock, pid.to_string()).expect("test");
            let holder = lock_holder(&lock).expect("test");
            prop_assert_eq!(holder, Some(LockHolder { pid, start: None }));
        }

        /// A pid and a start time read back as written.
        #[test]
        fn a_pid_and_start_read_back_unchanged(
            pid in 1..=i32::MAX.cast_unsigned(),
            start in 1_000_000_000u64..10_000_000_000,
        ) {
            let text = super::holder_text(pid, Some(start));
            prop_assert_eq!(parse_holder(&text), Some(LockHolder { pid, start: Some(start) }));
            let bare = super::holder_text(pid, None);
            prop_assert_eq!(parse_holder(&bare), Some(LockHolder { pid, start: None }));
        }

        /// Any text parses without a panic, and what parses is written back as the same words.
        #[test]
        fn what_parses_is_what_the_writer_writes(text in "\\PC{0,30}") {
            if let Some(holder) = parse_holder(&text) {
                let written = super::holder_text(holder.pid, holder.start);
                prop_assert_eq!(parse_holder(&written), Some(holder));
            }
        }
    }

    /// A start later than the recorded one by the tolerance is the same process. A start later
    /// by more than that is a new process. An earlier start is never a new process.
    #[test]
    fn only_a_start_past_the_tolerance_is_a_reused_pid() {
        let tolerance = super::START_TOLERANCE_SECS;
        assert!(!start_is_later(1_000, 1_000 + 2));
        assert!(!start_is_later(1_000, 1_000 + tolerance));
        assert!(start_is_later(1_000, 1_000 + tolerance + 1));
        assert!(!start_is_later(1_000 + tolerance + 1, 1_000));
    }

    /// A lock with a third word, or a start that is not a number, is not a lock of llmenv.
    #[test]
    fn a_malformed_lock_reads_as_no_holder() {
        for text in ["", "12 3 4", "x 3", "-1", "+12", "1_2", "0", "2147483648"] {
            assert_eq!(parse_holder(text), None, "{text:?}");
        }
    }

    /// A start that is cut short or is not a number leaves the pid, and no sign of reuse.
    #[test]
    fn a_start_that_is_not_a_full_epoch_time_is_dropped() {
        for text in ["12 17", "12 x", "12 -3", "12 +1700000000"] {
            assert_eq!(
                parse_holder(text),
                Some(LockHolder {
                    pid: 12,
                    start: None
                }),
                "{text:?}"
            );
        }
    }

    /// A lock cut inside its last character is an error, not a holder with a shorter pid. A pid
    /// that reads short could name a live process (#3508).
    #[test]
    fn a_lock_cut_inside_its_last_character_is_an_error() {
        let dir = tempfile::tempdir().expect("test");
        let lock = dir.path().join("proxy.lock");
        std::fs::write(&lock, b"4242\xe2\x82").expect("test");
        let err = lock_holder(&lock).expect_err("a cut lock must not read as a pid");
        assert!(
            format!("{err:#}").contains("reading proxy lockfile"),
            "{err:#}"
        );
    }

    /// The same rule for the pidfile: a cut pid is an error, not a shorter pid.
    #[test]
    fn a_pidfile_cut_inside_its_last_character_is_an_error() {
        let dir = tempfile::tempdir().expect("test");
        let pid = dir.path().join("proxy.pid");
        std::fs::write(&pid, b"4242\xe2\x82").expect("test");
        let err = super::super::read_pidfile(&pid).expect_err("a cut pidfile must not read");
        assert!(matches!(err, super::super::PidfileError::Io(_)), "{err:?}");
    }
}
