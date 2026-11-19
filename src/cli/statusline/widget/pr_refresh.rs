//! The detached refresh of a PR cache entry. The status line starts it and does not wait, so a
//! slow `gh` never holds up the prompt (#3889). Design: docs/design/statusline-pr-lookup.md

use serde::{Deserialize, Serialize};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use super::gh_cmd::GhPrCmd;
use super::now_unix;
use super::pr_cache::{pr_cache_path, read_pr_entry, refresh_in_place};
use super::usage_state_dir;

/// The input of the detached refresh, sent on its stdin. The branch name comes from git state, so
/// it travels in the payload and not in the process table. The cache folder is not part of it: the
/// child works it out from its own environment, so a job cannot point it at another folder.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct RefreshJob {
    repo_dir: PathBuf,
    branch: String,
}

/// Start the detached refresh of the cache entry for (`repo_dir`, `branch`), unless one runs.
///
/// The refresh holds a file lock next to the cache entry for as long as it lives, and the system
/// frees the lock when the process ends, so a child that dies leaves no lock behind. Two renders
/// that both see no lock start two children, and the second child finds the lock held and ends.
///
/// # Errors
/// Errors if the lock cannot be checked or the child cannot start. The caller shows the failure,
/// because a refresh that never starts would otherwise look like "no PR".
pub(super) fn start_refresh(cache_dir: &Path, repo_dir: &Path, branch: &str) -> anyhow::Result<()> {
    let path = pr_cache_path(cache_dir, repo_dir, branch);
    if refresh_is_running(&path)? {
        return Ok(());
    }
    let job = RefreshJob {
        repo_dir: repo_dir.to_path_buf(),
        branch: branch.to_string(),
    };
    spawn(&job)
}

/// The entry point of the hidden `statusline-pr-refresh` command: read the job from stdin, take the
/// lock, run `gh`, and write the cache entry. It ends at once when another refresh holds the lock.
///
/// # Errors
/// Errors if stdin is not a refresh job, or the cache folder or the lock is not available.
pub(crate) fn run() -> anyhow::Result<()> {
    // Starts the notice spool, so the notices of this child reach the next hook run (#3398).
    crate::cli::checkpoint_arg::checked_checkpoint(None)?;
    let payload_json = crate::cli::checkpoint_arg::read_job_payload()?;
    let job: RefreshJob = serde_json::from_str(&payload_json).map_err(|e| {
        anyhow::anyhow!(
            "the PR refresh input is not a refresh job: {e}. Fix: do not run this command by hand"
        )
    })?;
    let cache_dir = usage_state_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "the PR refresh has no state folder. Fix: set CLAUDE_CONFIG_DIR, as the engine does"
        )
    })?;
    let path = pr_cache_path(&cache_dir, &job.repo_dir, &job.branch);
    // The lock is held until this function ends.
    let Some(_lock) = take_lock(&path)? else {
        tracing::debug!("a PR refresh runs for {}, so this one ends", path.display());
        return Ok(());
    };
    let prior = read_pr_entry(&path);
    let now = now_unix();
    // The refresh that held the lock before this one may have written a fresh entry.
    if prior.as_ref().is_some_and(|entry| entry.is_fresh(now)) {
        return Ok(());
    }
    let (_lookup, _stale) = refresh_in_place(
        GhPrCmd::refresh("gh"),
        &path,
        (&job.repo_dir, &job.branch),
        prior,
        now,
    );
    Ok(())
}

fn lock_path(cache_path: &Path) -> PathBuf {
    let mut name = cache_path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Whether a refresh holds the lock of the cache entry at `cache_path`.
fn refresh_is_running(cache_path: &Path) -> anyhow::Result<bool> {
    crate::paths::create_dir_owner_only(cache_path.parent().unwrap_or(Path::new(".")))?;
    Ok(take_lock(cache_path)?.is_none())
}

/// The lock of the cache entry at `cache_path`, or `None` when another process holds it. The file
/// stays after the refresh, and it holds nothing: the lock is the system lock on the open file.
fn take_lock(cache_path: &Path) -> anyhow::Result<Option<File>> {
    let lock = lock_path(cache_path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    crate::paths::refuse_symlink(&mut options);
    let file = options.open(&lock).map_err(|e| {
        anyhow::anyhow!(
            "the PR refresh lock {} cannot be opened: {e}",
            lock.display()
        )
    })?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(anyhow::anyhow!(
            "the PR refresh lock {} cannot be taken: {e}",
            lock.display()
        )),
    }
}

/// A unit test runs in a test harness binary, which must not start as a refresh child. The
/// integration tests start the real child.
#[cfg(test)]
fn spawn(_job: &RefreshJob) -> anyhow::Result<()> {
    Ok(())
}

/// Spawn this binary as a detached child that runs the refresh. It is not waited on.
#[cfg(not(test))]
fn spawn(job: &RefreshJob) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    // Spawns this same binary. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = std::env::current_exe()?;
    let payload = serde_json::to_string(job)?;
    let mut cmd = Command::new(exe);
    cmd.arg("statusline-pr-refresh")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    crate::hook_run::respawn::spawn_detached(cmd, Some(&payload), &job.repo_dir)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_second_taker_finds_the_lock_held_until_the_first_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("pr-cache-x");
        let first = take_lock(&cache).unwrap();
        assert!(first.is_some());
        assert!(take_lock(&cache).unwrap().is_none(), "the lock is held");
        drop(first);
        assert!(
            take_lock(&cache).unwrap().is_some(),
            "a dropped lock is free"
        );
    }

    #[test]
    fn a_lock_that_cannot_be_opened_is_an_error_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("missing-folder").join("pr-cache-x");
        let err = take_lock(&cache).unwrap_err().to_string();
        assert!(err.contains("pr-cache-x.lock"), "{err}");
    }

    proptest! {
        // A refresh job survives the stdin hop, whatever the folder and the branch hold.
        #[test]
        fn a_refresh_job_round_trips(repo in "[a-zA-Z0-9/_. -]{0,40}", branch in "\\PC{0,40}") {
            let job = RefreshJob { repo_dir: PathBuf::from(repo), branch };
            let json = serde_json::to_string(&job).unwrap();
            prop_assert_eq!(serde_json::from_str::<RefreshJob>(&json).unwrap(), job);
        }
    }
}
