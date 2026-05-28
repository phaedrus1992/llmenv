use crate::git;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Sanitize a git remote URL for display, removing credentials.
/// Converts `https://user:pass@host/repo.git` to `https://[redacted]@host/repo.git`
#[allow(dead_code)]
fn sanitize_git_url(url: &str) -> String {
    if let Some(at_pos) = url.find('@')
        && let Some(scheme_end) = url.find("://")
        && at_pos > scheme_end
    {
        format!(
            "{}{}@{}",
            &url[..scheme_end + 3],
            "[redacted]",
            &url[at_pos + 1..]
        )
    } else {
        url.to_string()
    }
}

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
    std::fs::write(state_path(state_dir), secs.to_string())?;
    Ok(())
}

/// Throttled pull: check if interval has elapsed since last pull,
/// and if so, run `git fetch` followed by `git pull --ff-only` in repo.
/// Only updates state_dir if pull succeeds (to enable retry on failure).
pub fn maybe_pull(repo: &Path, state_dir: &Path, interval: Duration) -> Result<()> {
    let now = SystemTime::now();

    // Check if we should pull
    if let Some(last) = read_state(state_dir)?
        && now.duration_since(last).unwrap_or_default() < interval
    {
        return Ok(());
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
    // we don't want to spam every shell prompt while offline).
    let _ = git::secure_git()
        .args(["fetch"])
        .current_dir(repo)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Attempt fast-forward pull. Suppress git's stderr — we'll print our
    // own one-line warning on failure rather than git's two-line message.
    let pull_status = git::secure_git()
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
        // Some other pull failure (non-fast-forward, network, etc.). Don't
        // update state so we retry on next tick.
        tracing::debug!("git pull did not complete successfully; will retry on next pull interval");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sanitize_git_url_removes_credentials() {
        let url = "https://user:password@github.com/repo.git";
        let sanitized = sanitize_git_url(url);
        assert_eq!(sanitized, "https://[redacted]@github.com/repo.git");
        assert!(!sanitized.contains("password"));
    }

    #[test]
    fn sanitize_git_url_handles_no_credentials() {
        let url = "https://github.com/repo.git";
        assert_eq!(sanitize_git_url(url), url);
    }

    #[test]
    fn sanitize_git_url_handles_ssh() {
        let url = "git@github.com:alice/repo.git";
        let sanitized = sanitize_git_url(url);
        // SSH URLs have @ but are not credentials, they're just the git user
        assert_eq!(sanitized, url);
    }
}
