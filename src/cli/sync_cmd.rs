//! `llmenv sync`: commit and push the config repository, or report what a push would send.

use std::path::Path;

use anyhow::Context as _;

use crate::git;
use crate::paths;
use crate::sync::SyncOutcome;

pub(super) fn run_sync(dry_run: bool) -> anyhow::Result<()> {
    sync_in(&paths::config_dir()?, dry_run)
}

/// `llmenv sync` for the config repository in `config_dir`.
fn sync_in(config_dir: &Path, dry_run: bool) -> anyhow::Result<()> {
    if dry_run {
        eprintln!("{}", sync_dry_run_report(config_dir)?);
        return Ok(());
    }
    let outcome = crate::sync::commit_and_push(
        config_dir,
        "Update llmenv config",
        crate::sync::PushMode::Push,
    )?;
    eprintln!("{}", sync_outcome_message(&outcome));
    Ok(())
}

/// The report for a dry run: the pending changes, or a line that the tree is clean.
///
/// # Errors
/// `git status` cannot run, or it fails in `config_dir`.
fn sync_dry_run_report(config_dir: &Path) -> anyhow::Result<String> {
    let out = git::secure_git()
        .args(["status", "--short"])
        .current_dir(config_dir)
        .output()
        .context("failed to run git status")?;
    if !out.status.success() {
        let detail = llmenv_git::git_failure_detail(&out.stderr, &out.stdout, out.status);
        anyhow::bail!("git status failed: {detail}");
    }
    // A lossy decode would print a name that does not exist, so a non-UTF-8 name is an error.
    let output = String::from_utf8(out.stdout).context(
        "git status printed a file name that is not UTF-8. Fix: rename the file to a UTF-8 name.",
    )?;
    if output.trim().is_empty() {
        return Ok("Nothing to sync (working tree clean)".to_string());
    }
    Ok(format!("Would sync:\n{output}"))
}

/// The line that tells the user what a real sync did.
fn sync_outcome_message(outcome: &SyncOutcome) -> &'static str {
    match outcome {
        SyncOutcome::NothingToCommit => "No changes to commit (working tree clean)",
        SyncOutcome::Pushed => "✓ Synced config to GitHub",
        SyncOutcome::CommittedLocally => {
            "✓ Committed locally (remote sync disabled — push skipped)"
        }
    }
}

#[cfg(test)]
mod tests;
