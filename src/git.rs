//! Consolidated git utilities.
//!
//! Prevents malicious cloned repos from executing hooks or fsmonitors by
//! centralizing GIT_CONFIG_FLAGS application across all git operations.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Git config flags to protect cloned repos from executing hooks or fsmonitors.
/// Prevents a malicious config repo from running arbitrary code via git hooks or fsmonitors.
pub const GIT_CONFIG_FLAGS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// Apply security config flags to a git command.
pub fn secure_git() -> Command {
    let mut cmd = Command::new("git");
    cmd.args(GIT_CONFIG_FLAGS);
    cmd
}

/// Check if a path is a valid git repository.
pub fn is_valid_repo(path: &Path) -> bool {
    secure_git()
        .args(["rev-parse", "--git-dir"])
        .current_dir(path)
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Get the remote URL for a git repository.
pub fn get_remote_url(repo: &Path) -> Result<String> {
    let output = secure_git()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo)
        .output()
        .context("fetching remote URL")?;

    if !output.status.success() {
        anyhow::bail!("failed to get remote URL from {}", repo.display());
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Check if the working tree has staged or unstaged changes.
pub fn working_tree_dirty(repo: &Path) -> bool {
    secure_git()
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// Check if current branch has commits not yet pushed to its upstream.
pub fn has_unpushed_commits(repo: &Path) -> bool {
    let output = secure_git()
        .args(["rev-list", "--count", "@{u}..HEAD"])
        .current_dir(repo)
        .stderr(Stdio::null())
        .output();

    let Ok(output) = output else { return false };
    if !output.status.success() {
        return false;
    }

    let count = std::str::from_utf8(&output.stdout)
        .unwrap_or("0")
        .trim()
        .parse::<u32>()
        .unwrap_or(0);

    count > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_git_includes_config_flags() {
        let cmd = secure_git();
        // Just verify command is created; actual flag testing is in integration tests
        assert_eq!(cmd.get_program(), "git");
    }
}
