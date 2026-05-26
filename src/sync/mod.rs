use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    // Attempt fetch — log but don't fail on fetch errors (network issues are transient)
    if let Err(e) = Command::new("git")
        .args(["fetch"])
        .current_dir(repo)
        .status()
        .context("git fetch failed")
    {
        tracing::warn!("git fetch error (continuing with local pull): {e}");
    }

    // Attempt fast-forward pull — fail on merge conflicts or critical errors
    let pull_status = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(repo)
        .status()
        .context("git pull --ff-only failed")?;

    // Only update state if pull succeeded (exit code 0)
    if pull_status.success() {
        write_state(state_dir, now)?;
    } else {
        tracing::warn!("git pull did not complete successfully; will retry on next pull interval");
    }

    Ok(())
}
