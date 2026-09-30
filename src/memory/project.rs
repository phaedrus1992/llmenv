//! The session's project name, as ICM derives it (#2249).
//!
//! ICM filters `icm_wake_up` and `icm_memory_recall` by project name, and without
//! a `project` argument it uses the ICM server's own working directory. For a
//! remote `icm serve` that directory has nothing to do with the session, so
//! llmenv sends the name itself, using ICM's rule: the origin remote's repo name,
//! then the main repository's directory name, then the path's base name.

use std::path::Path;

use crate::git::secure_git;

/// The repository name at the end of a git remote URL: HTTPS, `user@host:path`,
/// or an SSH alias such as `alias:owner/repo.git`.
fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last_segment = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = last_segment.strip_suffix(".git").unwrap_or(last_segment);
    (!name.is_empty()).then(|| name.to_string())
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = secure_git().args(args).current_dir(dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The project name for a session whose working directory is `cwd`.
pub(crate) fn session_project(cwd: &Path) -> Option<String> {
    if let Some(name) = git_output(cwd, &["config", "--get", "remote.origin.url"])
        .as_deref()
        .and_then(repo_name_from_url)
    {
        return Some(name);
    }
    // The common dir is the main repo's `.git`, also from a linked worktree.
    if let Some(name) = git_output(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .and_then(|common| std::fs::canonicalize(common).ok())
    .and_then(|common| {
        common
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
    }) {
        return Some(name);
    }
    cwd.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn repo_name_from_url_reads_https_ssh_and_scp_forms() {
        for (url, name) in [
            ("https://github.com/phaedrus1992/llmenv.git", Some("llmenv")),
            ("https://github.com/phaedrus1992/llmenv/", Some("llmenv")),
            ("git@github.com:phaedrus1992/llmenv.git", Some("llmenv")),
            ("github-phaedrus:phaedrus1992/llmenv.git", Some("llmenv")),
            ("git@host:repo.git", Some("repo")),
            ("", None),
            ("/", None),
        ] {
            assert_eq!(repo_name_from_url(url).as_deref(), name, "{url}");
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn session_project_prefers_the_origin_repo_name() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("renamed-checkout");
        std::fs::create_dir(&checkout).unwrap();
        git(&checkout, &["init", "-q"]);
        git(
            &checkout,
            &["remote", "add", "origin", "git@github.com:me/real-name.git"],
        );
        let sub = checkout.join("src");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(session_project(&sub).as_deref(), Some("real-name"));
    }

    #[test]
    fn session_project_uses_the_repo_dir_without_a_remote() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("local-repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let sub = repo.join("deep");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(session_project(&sub).as_deref(), Some("local-repo"));
    }

    #[test]
    fn session_project_falls_back_to_the_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain-dir");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(session_project(&plain).as_deref(), Some("plain-dir"));
        assert_eq!(session_project(Path::new("/")), None);
    }
}
