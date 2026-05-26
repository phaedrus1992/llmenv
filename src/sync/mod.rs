use anyhow::Result;
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
/// Updates state_dir on success (or after fetch attempt, regardless of merge success).
pub fn maybe_pull(repo: &Path, state_dir: &Path, interval: Duration) -> Result<()> {
    let now = SystemTime::now();

    // Check if we should pull
    if let Some(last) = read_state(state_dir)?
        && now.duration_since(last).unwrap_or_default() < interval
    {
        return Ok(());
    }

    // Attempt fetch
    let _ = Command::new("git")
        .args(["fetch"])
        .current_dir(repo)
        .status();

    // Attempt fast-forward pull
    let _ = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(repo)
        .status();

    // Update state after pull attempt (success or fail)
    write_state(state_dir, now)?;

    Ok(())
}
