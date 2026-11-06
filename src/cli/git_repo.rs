//! Whether a directory is inside a git repository.

use std::path::Path;

use anyhow::Context;

use crate::git;

/// Whether `dir` is inside a git repository.
///
/// # Errors
/// git cannot start, or `git rev-parse` fails for a reason other than "not a git repository",
/// such as dubious ownership. That failure must not read as "no repo" (#2710).
pub(super) fn is_git_repo(dir: &Path) -> anyhow::Result<bool> {
    let output = git::local_query(dir, &["rev-parse", "--git-dir"])
        .with_context(|| format!("failed to run git in {}", dir.display()))?;
    if output.status.success() {
        return Ok(true);
    }
    if git::is_outside_repo(
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
    ) {
        return Ok(false);
    }
    anyhow::bail!(
        "git could not check {}: {}",
        dir.display(),
        git::git_failure_detail(&output.stderr, &output.stdout, output.status)
    )
}
