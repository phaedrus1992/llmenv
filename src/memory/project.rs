//! The session's project name, as ICM derives it (#2249).
//!
//! ICM filters `icm_wake_up` and `icm_memory_recall` by project name, and without
//! a `project` argument it uses the ICM server's own working directory. For a
//! remote `icm serve` that directory has nothing to do with the session, so
//! llmenv sends the name itself, using ICM's rule: the origin remote's repo name,
//! then the main repository's directory name, then the path's base name.

use std::path::Path;

use anyhow::Context as _;

/// The repository name at the end of a git remote URL: HTTPS, `user@host:path`,
/// or an SSH alias such as `alias:owner/repo.git`.
fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last_segment = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = last_segment.strip_suffix(".git").unwrap_or(last_segment);
    // A name with `@` is userinfo, such as `https://user:TOKEN@host`. Storing it would keep the
    // credential as the project name, so it is not a name.
    (!name.is_empty() && !name.contains('@')).then(|| name.to_string())
}

/// The trimmed stdout of a successful git run. `None` when the output is empty.
///
/// # Errors
/// The output is not UTF-8.
fn decode_git_stdout(dir: &Path, stdout: Vec<u8>) -> anyhow::Result<Option<String>> {
    let text = String::from_utf8(stdout).with_context(|| {
        format!(
            "git in {} printed non-UTF-8 output. Check the repository's configuration.",
            dir.display()
        )
    })?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// The trimmed stdout of `git args` in `dir`. `None` is a normal answer: git exits 1 when the key
/// is absent, or the directory is not inside a repository.
///
/// # Errors
/// git cannot start in `dir`, or it fails for any other reason, such as "dubious ownership" or an
/// unreadable `.git`. Any other failure would otherwise pick a different project name silently.
fn git_output(dir: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    // A hook waits for this, so a git that hangs on a stuck mount must not hang the hook (#2923).
    let output = crate::git::local_query(dir, args).map_err(|e| {
        anyhow::anyhow!(
            "cannot run git in {} to name the project: {e}. Check that git is installed and on \
             PATH, and that the folder is reachable.",
            dir.display()
        )
    })?;
    if output.status.success() {
        return decode_git_stdout(dir, output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `git config --get` exits 1 when the key is absent, which is a normal answer. Other git
    // commands use exit 1 for a real failure, so only a config lookup counts as absent (#3053).
    let absent_key = args.first() == Some(&"config") && output.status.code() == Some(1);
    if absent_key || crate::git::is_not_a_repo_message(&stderr) {
        return Ok(None);
    }
    anyhow::bail!(
        "git in {} could not name the project ({}): {}. Check that the repository's `.git` \
         directory is readable and owned by you.",
        dir.display(),
        output.status,
        llmenv_git::git_failure_detail(&output.stderr, &output.stdout, output.status)
    )
}

/// The project name for a session whose working directory is `cwd`.
///
/// # Errors
/// git cannot start in `cwd`, fails for a reason other than "not a repository", the git
/// common directory cannot be resolved, or `cwd` has no name to fall back to. The caller decides
/// what the agent sees.
pub(crate) fn session_project(cwd: &Path) -> anyhow::Result<String> {
    if let Some(url) = git_output(cwd, &["config", "--get", "remote.origin.url"])? {
        // A remote that is set but has no readable name is a config problem. Falling back to the
        // folder name would hide it. The URL is masked, so credentials stay out of the error.
        let name = repo_name_from_url(&url).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot read a repo name from the origin URL {}. \
                 Set a valid origin with `git remote set-url origin <url>`.",
                crate::util::strip_unsafe_chars(&crate::git::sanitize_git_url(&url))
            )
        })?;
        return Ok(name);
    }
    // The common dir is the main repo's `.git`, also from a linked worktree.
    if let Some(common) = git_output(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )? {
        let common = std::fs::canonicalize(&common).with_context(|| {
            format!("cannot resolve the git common directory {common}. Check the worktree's link.")
        })?;
        let name = common.parent().and_then(Path::file_name).ok_or_else(|| {
            anyhow::anyhow!("git common directory {} has no parent", common.display())
        })?;
        return Ok(name.to_string_lossy().into_owned());
    }
    let name = cwd.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot tell the project of {}: the folder has no name. Run this from a folder with \
             a name, or inside a git repository.",
            cwd.display()
        )
    })?;
    Ok(name.to_string_lossy().into_owned())
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
            ("https://user:TOKEN@host", None),
            ("https://user:TOKEN@host/", None),
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

    /// #3053: `git diff --quiet` exits 1 on a dirty tree. Only a config lookup treats exit 1 as
    /// absent, so this failure must surface as an error.
    #[test]
    fn a_git_failure_outside_config_is_an_error_not_an_absent_name() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        // The fixture repo sets its own identity. Signing and hooks come from the user's global
        // config, so the fixture turns both off in its own repo.
        git(dir.path(), &["config", "user.name", "t"]);
        git(dir.path(), &["config", "user.email", "t@t"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.hooksPath", "/dev/null"]);
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "one").unwrap();
        git(dir.path(), &["add", "f.txt"]);
        git(dir.path(), &["commit", "-qm", "init"]);
        // A longer body, so git sees a size change even within the same timestamp tick.
        std::fs::write(&file, "two, changed").unwrap();

        let result = git_output(dir.path(), &["diff", "--quiet"]);
        let message = match &result {
            Ok(_) => String::new(),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            result.is_err(),
            "a dirty tree makes diff --quiet exit 1, a real failure: {result:?}"
        );
        assert!(
            message.contains("could not name the project"),
            "the error must name the failed lookup: {message}"
        );
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
        assert_eq!(session_project(&sub).ok().as_deref(), Some("real-name"));
    }

    #[test]
    fn session_project_errors_on_an_unparseable_origin_and_masks_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("odd-remote");
        std::fs::create_dir(&checkout).unwrap();
        git(&checkout, &["init", "-q"]);
        git(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                // The last path segment is ".git", so the parser finds no repo name.
                "https://user:secret@/.git",
            ],
        );
        let err = session_project(&checkout).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("origin URL"), "unexpected error: {text}");
        assert!(!text.contains("secret"), "credentials leaked into: {text}");
    }

    #[test]
    fn session_project_uses_the_repo_dir_without_a_remote() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("local-repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let sub = repo.join("deep");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(session_project(&sub).ok().as_deref(), Some("local-repo"));
    }

    #[test]
    fn decode_git_stdout_trims_and_maps_blank_to_none() {
        let dir = Path::new("/r");
        assert_eq!(
            decode_git_stdout(dir, b"  x \n".to_vec())
                .ok()
                .flatten()
                .as_deref(),
            Some("x")
        );
        assert_eq!(decode_git_stdout(dir, b" \n".to_vec()).unwrap(), None);
        assert!(decode_git_stdout(dir, vec![0xff, 0xfe]).is_err());
    }

    proptest::proptest! {
        #[test]
        fn decode_git_stdout_keeps_any_utf8_text_trimmed(s in ".*") {
            let got = decode_git_stdout(Path::new("/r"), s.clone().into_bytes()).unwrap();
            let trimmed = s.trim();
            proptest::prop_assert_eq!(got.as_deref(), (!trimmed.is_empty()).then_some(trimmed));
        }
    }

    #[test]
    fn session_project_falls_back_to_the_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain-dir");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(session_project(&plain).ok().as_deref(), Some("plain-dir"));
    }

    #[test]
    fn session_project_errors_for_a_directory_with_no_name() {
        // A cwd with no file name, such as `/`, has no project. Returning None here sent a
        // recall with no project scope, so icm searched the server's own folder (#2768).
        let err = session_project(Path::new("/")).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("/") && text.contains("no name"),
            "the error must name the directory and the cause: {text}"
        );
    }

    #[test]
    fn session_project_errors_when_git_cannot_start_in_the_directory() {
        let missing = Path::new("/no/such/dir/for/llmenv-project");
        let err = session_project(missing).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("cannot run git") && text.contains("/no/such/dir/for/llmenv-project"),
            "the error must name git and the directory: {text}"
        );
    }

    mod props {
        use super::super::repo_name_from_url;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn the_name_is_one_non_empty_path_segment(url in "\\PC{0,60}") {
                if let Some(name) = repo_name_from_url(&url) {
                    prop_assert!(!name.is_empty());
                    prop_assert!(!name.contains('/') && !name.contains(':'));
                }
            }

            #[test]
            fn a_well_formed_url_yields_its_repo_name(
                owner in "[a-z][a-z0-9-]{0,12}",
                repo in "[a-z][a-z0-9_-]{0,20}",
                form in 0usize..3,
            ) {
                let url = match form {
                    0 => format!("https://github.com/{owner}/{repo}.git"),
                    1 => format!("git@github.com:{owner}/{repo}.git"),
                    _ => format!("alias:{owner}/{repo}/"),
                };
                prop_assert_eq!(repo_name_from_url(&url), Some(repo));
            }
        }
    }
}
