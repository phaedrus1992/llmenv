pub use llmenv_git::{
    DEFAULT_GIT_PLUGIN_TIMEOUT_SECS, DEFAULT_GIT_TIMEOUT_SECS, GIT_CONFIG_FLAGS, apply_git_timeout,
    git_failure_detail, has_unpushed_commits, sanitize_git_url, secure_git, working_tree_dirty,
};

/// How long a local git query may run, such as `rev-parse` or `symbolic-ref`. A local git answers
/// in milliseconds, so a longer wait means a stuck mount, and a hook must not wait on it (#2923).
const LOCAL_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run the local git query `args` in `dir`, with the English messages that callers match on, for
/// at most [`LOCAL_QUERY_TIMEOUT`].
///
/// # Errors
/// git cannot start, or it runs past the timeout.
pub(crate) fn local_query(
    dir: &std::path::Path,
    args: &[&str],
) -> std::io::Result<std::process::Output> {
    let mut cmd = secure_git();
    cmd.args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::process::run_traced("git:local-query", cmd, LOCAL_QUERY_TIMEOUT)
}

/// Whether a failed git command ran in a folder outside a repository: git's fatal exit, 128, with
/// "not a git repository" in stderr. The phrase from any other exit, or from a kill by signal
/// (`None`), is a real failure (#3433).
///
/// Git may capitalise the message, so the match ignores case (#3040).
pub(crate) fn is_outside_repo(code: Option<i32>, stderr: &str) -> bool {
    code == Some(128) && stderr.to_ascii_lowercase().contains("not a git repository")
}
