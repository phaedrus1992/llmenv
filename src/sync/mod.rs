use crate::git;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing;

/// True if the repo's working tree has staged or unstaged changes.
fn working_tree_dirty(repo: &Path) -> bool {
    git::working_tree_dirty(repo)
}

/// True if the current branch has commits not yet pushed to its upstream.
/// Returns false if there's no upstream or git fails — we only want to nudge
/// the user when we're certain.
fn has_unpushed_commits(repo: &Path) -> bool {
    git::has_unpushed_commits(repo)
}

/// Result of [`commit_and_push`]: whether a commit was actually pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// A commit was created and pushed to origin.
    Pushed,
    /// The working tree was clean — nothing to commit or push.
    NothingToCommit,
}

/// Run a git subcommand in `repo`, capturing its output. On a non-zero exit the
/// captured stderr is surfaced in the returned error so failures are loud, and
/// capturing (rather than inheriting) stdout/stderr keeps git's chatter out of
/// a piped `llmenv export` eval context (#307).
///
/// # Errors
/// Returns an error if git cannot be spawned or exits non-zero (stderr included).
fn run_git_checked(repo: &Path, args: &[&str], what: &str) -> Result<()> {
    let mut cmd = git::secure_git();
    let output = git::apply_git_timeout(&mut cmd, git::DEFAULT_GIT_TIMEOUT_SECS)
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to spawn git to {what}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to {what}: {}",
            git::git_failure_detail(&output.stderr, &output.stdout, output.status)
        );
    }
    Ok(())
}

/// Stage and commit every change in `repo`, and optionally push to origin.
///
/// "Nothing to commit" is detected up front by inspecting the working tree
/// after staging — not by misreading `git commit`'s exit code — so a commit
/// that fails for a real reason (e.g. missing identity) surfaces as an error
/// instead of being mistaken for a clean tree. A failed `git push` is likewise
/// surfaced rather than silently treated as success (#307).
///
/// When `push` is `false`, the add + commit step still runs — local history is
/// preserved — but the remote push is skipped. This lets users disable remote
/// git operations (e.g. when 1Password is locked) while keeping local commits.
///
/// # Errors
/// Returns an error if any git step fails to spawn or exits non-zero.
pub fn commit_and_push(repo: &Path, message: &str, push: bool) -> Result<SyncOutcome> {
    run_git_checked(repo, &["add", "-A"], "stage changes (git add -A)")?;

    // After staging, an empty `status --porcelain` means there is genuinely
    // nothing to commit — distinct from `git commit` failing for another reason.
    if !working_tree_dirty(repo) {
        return Ok(SyncOutcome::NothingToCommit);
    }

    run_git_checked(
        repo,
        &["commit", "-m", message],
        "create commit (git commit)",
    )?;
    if push {
        run_git_checked(repo, &["push"], "push config (git push)")?;
        Ok(SyncOutcome::Pushed)
    } else {
        // Local-only commit — no push. Return Pushed anyway; the commit was
        // made and will be pushed on a subsequent run when remote_sync is
        // re-enabled.
        Ok(SyncOutcome::Pushed)
    }
}

/// Path to the sync state file within state_dir.
pub fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("sync.json")
}

/// Read the last-pull timestamp from state_dir.
/// Returns Ok(None) if the file doesn't exist, Ok(Some(time)) if it does.
pub fn read_state(state_dir: &Path) -> Result<Option<SystemTime>> {
    let p = state_path(state_dir);
    if !p.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&p)?;
    let secs: u64 = s.trim().parse()?;
    Ok(Some(UNIX_EPOCH + Duration::from_secs(secs)))
}

/// Write the current pull timestamp to state_dir/sync.json.
pub fn write_state(state_dir: &Path, t: SystemTime) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let secs = t.duration_since(UNIX_EPOCH)?.as_secs();
    crate::paths::write_owner_only_atomic(&state_path(state_dir), secs.to_string().as_bytes())?;
    Ok(())
}

/// Throttled pull: check if interval has elapsed since last pull,
/// and if so, run `git fetch` followed by `git pull --ff-only` in repo.
/// Only updates state_dir if pull succeeds (to enable retry on failure).
pub fn maybe_pull(repo: &Path, state_dir: &Path, interval: Duration) -> Result<()> {
    let now = SystemTime::now();

    if let Some(last) = read_state(state_dir)? {
        match now.duration_since(last) {
            Ok(elapsed) if elapsed < interval => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    skew_secs = e.duration().as_secs(),
                    "system clock skew detected (state timestamp {}s in future); proceeding with pull",
                    e.duration().as_secs()
                );
            }
            Ok(_) => {}
        }
    }

    // Validate repo is a git repository
    if !repo.join(".git").exists() {
        return Err(anyhow::anyhow!(
            "config directory is not a git repository: {}",
            repo.display()
        ));
    }

    // Working tree dirty → don't try to pull (git will refuse on rebase, and
    // a fast-forward could clobber uncommitted edits anyway). Surface a
    // one-line nudge and return early; treat this as success so we don't
    // retry every shell prompt.
    if working_tree_dirty(repo) {
        eprintln!(
            "llmenv: config in {} has uncommitted changes — run `llmenv sync` to commit and push",
            repo.display()
        );
        write_state(state_dir, now)?;
        return Ok(());
    }

    // Attempt fetch — silent on failure (network issues are transient and
    // we don't want to spam every shell prompt while offline). A spawn error
    // (git binary missing or broken) is unexpected and warrants a warning.
    // Apply a short timeout to prevent freezing on stuck remotes (#449).
    let mut fetch_cmd = git::secure_git();
    if let Err(e) = git::apply_git_timeout(&mut fetch_cmd, git::DEFAULT_GIT_TIMEOUT_SECS)
        .args(["fetch"])
        .current_dir(repo)
        .stderr(std::process::Stdio::null())
        .status()
    {
        tracing::warn!("git fetch failed to start in {}: {}", repo.display(), e);
    }

    // Attempt fast-forward pull. Suppress git's stderr — we'll print our
    // own one-line warning on failure rather than git's two-line message.
    // Apply a short timeout to prevent freezing on stuck remotes (#449).
    let mut pull_cmd = git::secure_git();
    let pull_status = git::apply_git_timeout(&mut pull_cmd, git::DEFAULT_GIT_TIMEOUT_SECS)
        .args(["pull", "--ff-only"])
        .current_dir(repo)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context(format!("git pull --ff-only failed in {}", repo.display()))?;

    if pull_status.success() {
        write_state(state_dir, now)?;
    } else if has_unpushed_commits(repo) {
        eprintln!(
            "llmenv: config in {} has unpushed commits — run `llmenv sync` to push",
            repo.display()
        );
        write_state(state_dir, now)?;
    } else {
        // Some other pull failure (non-fast-forward, diverged, conflict, auth).
        // Update state so a clock-skew event followed by a pull failure doesn't
        // leave the timestamp stuck and trigger a pull attempt on every shell
        // prompt forever — the nudge below is the user's cue to intervene (#386).
        write_state(state_dir, now)?;
        eprintln!(
            "llmenv: config in {} could not fast-forward (diverged or network error) — \
             run `llmenv sync` for details",
            repo.display()
        );
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    #[test]
    fn git_config_flags_protect_against_hooks() {
        use crate::git::GIT_CONFIG_FLAGS;
        assert_eq!(
            GIT_CONFIG_FLAGS,
            &[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null"
            ]
        );
    }

    #[test]
    fn write_state_then_read_state_roundtrips() {
        use super::{read_state, write_state};
        use std::time::{Duration, SystemTime};
        let tmp = tempfile::tempdir().unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write_state(tmp.path(), t).unwrap();
        let got = read_state(tmp.path()).unwrap().unwrap();
        // Round-trip through seconds; sub-second precision is not preserved.
        assert_eq!(
            got.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
        );
    }
}
