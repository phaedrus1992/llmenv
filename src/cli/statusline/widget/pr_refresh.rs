//! The detached refresh of a PR cache entry. The status line starts it and does not wait, so a
//! slow `gh` never holds up the prompt (#3889). Design: docs/design/statusline-pr-lookup.md

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::gh_cmd::GhPrCmd;
use super::now_unix;
use super::pr_cache::{pr_cache_path, read_pr_entry, refresh_in_place};

/// How old a refresh lock may be before it is taken as left by a child that died. It is longer
/// than the time the child may take: the limit of `gh`, and the retry after a failed start.
const REFRESH_LOCK_STALE_SECS: i64 = 30;

/// The input of the detached refresh, sent on its stdin. The branch name comes from git state, so
/// it travels in the payload and not in the process table.
#[derive(Serialize, Deserialize)]
pub(crate) struct RefreshJob {
    cache_dir: PathBuf,
    repo_dir: PathBuf,
    branch: String,
}

/// Start the detached refresh of the cache entry for (`repo_dir`, `branch`), unless one runs. One
/// lock file next to the entry keeps two renders from starting two refreshes. A refresh that
/// cannot start is logged, and the next render tries again.
pub(super) fn start_refresh(cache_dir: &Path, repo_dir: &Path, branch: &str, now: i64) {
    if let Err(e) = crate::paths::create_dir_owner_only(cache_dir) {
        tracing::error!(
            "pr cache dir {} unavailable, so the PR is not refreshed: {e}",
            cache_dir.display()
        );
        return;
    }
    let lock = lock_path(&pr_cache_path(cache_dir, repo_dir, branch));
    if !take_lock(&lock, now) {
        return;
    }
    let job = RefreshJob {
        cache_dir: cache_dir.to_path_buf(),
        repo_dir: repo_dir.to_path_buf(),
        branch: branch.to_string(),
    };
    if let Err(e) = spawn(&job) {
        tracing::error!(
            "the detached PR refresh cannot start, so the PR is not refreshed: {e:#}. Fix: run \
             `llmenv doctor`."
        );
        drop_lock(&lock);
    }
}

/// The entry point of the hidden `statusline-pr-refresh` command: read the job from stdin, run
/// `gh`, write the cache entry, and release the lock.
///
/// # Errors
/// Errors if stdin is not a refresh job.
pub(crate) fn run() -> anyhow::Result<()> {
    // Starts the notice spool, so the notices of this child reach the next hook run (#3398).
    crate::cli::checkpoint_arg::checked_checkpoint(None)?;
    let payload_json = crate::cli::checkpoint_arg::read_job_payload()?;
    let job: RefreshJob = serde_json::from_str(&payload_json).map_err(|e| {
        anyhow::anyhow!(
            "the PR refresh input is not a refresh job: {e}. Fix: do not run this command by hand"
        )
    })?;
    let path = pr_cache_path(&job.cache_dir, &job.repo_dir, &job.branch);
    let prior = read_pr_entry(&path);
    let (_lookup, _stale) = refresh_in_place(
        GhPrCmd::refresh("gh"),
        &path,
        (&job.repo_dir, &job.branch),
        prior,
        now_unix(),
    );
    drop_lock(&lock_path(&path));
    Ok(())
}

pub(super) fn lock_path(cache_path: &Path) -> PathBuf {
    let mut name = cache_path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Create the lock. `false` when a refresh holds it, or when it cannot be made.
pub(super) fn take_lock(lock: &Path, now: i64) -> bool {
    match create_lock(lock) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !lock_is_stale(lock, now) {
                return false;
            }
            tracing::warn!(
                "the PR refresh lock {} is old, so a child died. It is replaced.",
                lock.display()
            );
            drop_lock(lock);
            create_lock(lock).is_ok()
        }
        Err(e) => {
            tracing::error!(
                "the PR refresh lock {} cannot be made, so the PR is not refreshed: {e}",
                lock.display()
            );
            false
        }
    }
}

fn create_lock(lock: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(lock).map(drop)
}

fn lock_is_stale(lock: &Path, now: i64) -> bool {
    let modified = std::fs::metadata(lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok());
    // A lock whose age cannot be read is taken as old, so a broken lock cannot stop refreshes.
    modified.is_none_or(|at| now.saturating_sub(at) >= REFRESH_LOCK_STALE_SECS)
}

fn drop_lock(lock: &Path) {
    if let Err(e) = std::fs::remove_file(lock)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::error!(
            "the PR refresh lock {} cannot be removed: {e}",
            lock.display()
        );
    }
}

/// A unit test runs in a test harness binary, which must not start as a refresh child. The
/// integration tests start the real child.
#[cfg(test)]
fn spawn(_job: &RefreshJob) -> anyhow::Result<()> {
    anyhow::bail!("the detached refresh is off in unit tests")
}

/// Spawn this binary as a detached child that runs the refresh. It is not waited on.
#[cfg(not(test))]
fn spawn(job: &RefreshJob) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    // Spawns this same binary. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = std::env::current_exe()?;
    let payload = serde_json::to_string(job)?;
    let mut cmd = Command::new(exe);
    cmd.arg("statusline-pr-refresh")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    crate::hook_run::redirect_stderr_to_detached_log(
        &mut cmd,
        crate::hook_run::detached_child_log_path(),
    );
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let mut child = crate::trace::spawn_counted("statusline:pr-refresh", &mut cmd)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes())?;
    }
    Ok(())
}
