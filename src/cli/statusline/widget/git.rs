//! Git branch and worktree lookup for the branch and pr widgets.

use super::*;

/// Resolve the current branch, in precedence order:
///
/// 1. `branch.name` from stdin — forward-compat if the engine ever sends it.
/// 2. `worktree.branch` from stdin — Claude Code's only branch field, present
///    for `--worktree` sessions.
/// 3. Git, derived from `workspace.current_dir` — Claude Code sends **no
///    branch** for a regular repo (confirmed against the statusline docs), so
///    llmenv reads it from `.git/HEAD` rather than leaving the widget blank.
///
/// Shared by [`render_branch`] and the `pr` widget's self-resolving fallback
/// ([`resolve_pr`]) — both need "what branch is this" before they can do
/// anything engine-independent.
pub(super) fn resolve_branch_name(data: &EngineData) -> Result<Option<String>, String> {
    let named = data
        .branch
        .as_ref()
        .and_then(|b| b.name.clone())
        .or_else(|| data.worktree.as_ref().and_then(|w| w.branch.clone()));
    if named.is_some() {
        return Ok(named);
    }
    data.workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
        .map_or(Ok(None), |dir| git_branch(Path::new(dir)))
}

pub(super) fn render_branch(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    render_branch_with(data, cfg, use_color, GhPrCmd::new("gh"))
}

pub(super) fn render_branch_with(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
    gh: GhPrCmd<'_>,
) -> String {
    let name = match resolve_branch_name(data) {
        Ok(Some(name)) => name,
        Ok(None) => return String::new(),
        // The segment keeps its format and shows the marker, so it is never hidden silently
        // (#3745).
        Err(e) => {
            tracing::error!("statusline branch cannot be read: {e}");
            return super::super::fill_unknown(cfg, "\u{1f33f} {name}", &["{name}"]);
        }
    };
    let name = super::super::sanitize(&name); // untrusted (stdin / git ref)
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("\u{1f33f} {name}"); // 🌿
    let label = format.replace("{name}", &name);
    // Link the branch to its PR (OSC 8). Route through resolve_pr so the link
    // derives the PR when the engine sends none (Claude Code), matching {pr}.
    // A read error is already shown by the branch label above, so it adds no link.
    let pr = match resolve_pr_with(data, gh) {
        Ok(pr) => pr,
        // The branch label above already shows a branch read error, and the pr widget shows a
        // gh failure, so the link is dropped.
        Err(e) => {
            tracing::debug!("statusline branch link skipped: {e:?}");
            None
        }
    };
    let pr_url = pr
        .and_then(|p| p.url)
        .map(|u| super::super::sanitize(&u))
        .unwrap_or_default();
    if use_color && super::super::valid_url(&pr_url) {
        super::super::hyperlink(&label, &pr_url)
    } else {
        label
    }
}

/// Current branch by reading `.git/HEAD` directly (no `git` subprocess — the
/// statusline re-renders on every UI tick, and the codebase already avoids
/// per-render forks). Returns `Ok(None)` for a detached HEAD or when no repository
/// encloses `dir`.
///
/// # Errors
/// `HEAD` exists in a repository but cannot be read (#2710).
pub(super) fn git_branch(dir: &Path) -> Result<Option<String>, String> {
    let Some(git_dir) = find_git_dir(dir)? else {
        return Ok(None);
    };
    let head_path = git_dir.join("HEAD");
    // The repository can be a cloned one, so the read refuses a FIFO or a huge file (#2945).
    let head = match crate::paths::bounded::state_text_io(&head_path) {
        Ok(head) => head,
        // A pruned worktree points at a gitdir that is gone. That is no branch, not a read error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", head_path.display())),
    };
    Ok(head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string))
}

/// Locate the git directory enclosing `start`, walking up its ancestors.
/// Handles both a `.git` directory (normal clone) and a `.git` *file* holding
/// `gitdir: <path>` (a linked worktree or submodule).
pub(super) fn find_git_dir(start: &Path) -> Result<Option<PathBuf>, String> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        let meta = match std::fs::symlink_metadata(&dot_git) {
            Ok(meta) => meta,
            // Only a missing `.git` means this folder has no repository. Any other error is
            // returned, so an unreadable repository is not read as an ancestor's repository.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", dot_git.display())),
        };
        // git accepts a `.git` link to a git directory, so a link is followed for that check.
        let is_dir = meta.is_dir() || (meta.is_symlink() && dot_git.is_dir());
        if is_dir {
            return Ok(Some(dot_git));
        }
        // `.git` file: `gitdir: <path>` pointer (worktree/submodule). A file that cannot be read
        // or has no pointer is an error, not "no repository", so the branch label can say so.
        // The bounded read refuses a FIFO, which would block every render (#2945).
        let content = crate::paths::bounded::state_text_io(&dot_git)
            .map_err(|e| format!("cannot read {}: {e}", dot_git.display()))?;
        let gitdir = content
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| format!("{} has no gitdir pointer", dot_git.display()))?;
        // A submodule pointer is relative to the `.git` file, not to the process cwd. `join`
        // keeps an absolute pointer as it is.
        return Ok(Some(dir.join(gitdir)));
    }
    Ok(None)
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_parent_is_an_error_not_an_ancestor_repository() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        let inner = locked.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = find_git_dir(&inner);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "an unreadable path must be an error: {result:?}"
        );
    }

    fn mkfifo(path: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success());
    }

    #[test]
    fn a_dot_git_link_to_a_git_directory_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.git");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::os::unix::fs::symlink(&real, repo.join(".git")).unwrap();
        assert_eq!(git_branch(&repo).unwrap().as_deref(), Some("main"));
    }

    /// A FIFO at `.git` or at `HEAD` is an error at once, so a render does not block.
    #[test]
    fn a_fifo_dot_git_or_head_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        mkfifo(&repo.join(".git"));
        assert!(find_git_dir(&repo).unwrap_err().contains(".git"));

        let other = dir.path().join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        mkfifo(&other.join(".git").join("HEAD"));
        assert!(git_branch(&other).unwrap_err().contains("HEAD"));
    }
}
