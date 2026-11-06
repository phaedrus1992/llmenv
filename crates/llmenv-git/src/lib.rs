//! Consolidated git utilities.
//!
//! Prevents malicious cloned repos from executing hooks or fsmonitors by
//! centralizing GIT_CONFIG_FLAGS application across all git operations.

use std::path::Path;
use std::process::{Command, Stdio};

/// The program that [`secure_git`] runs when no absolute `PATH` folder holds `git`. The path
/// does not exist, so the spawn fails as not found and names no relative folder.
const GIT_NOT_FOUND: &str = "/git-is-not-in-an-absolute-PATH-folder/git";

/// Git config flags to protect cloned repos from executing hooks or fsmonitors.
/// Prevents a malicious config repo from running arbitrary code via git hooks or fsmonitors.
pub const GIT_CONFIG_FLAGS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// Apply security config flags to a git command, with stdin detached.
///
/// No git operation in this codebase reads from stdin, so stdin is nulled at
/// construction: a command invoked with a non-interactive stdin (CI, or the
/// `source <(llmenv export)` eval context) can never block on an interactive
/// prompt such as a credential helper (#299, #307). Centralizing it here means
/// every `secure_git()` call site — including `.status()`/`.spawn()` callers
/// that would otherwise inherit the parent's stdin — gets the guarantee.
///
/// `git` is taken from an absolute `PATH` folder only, because a bare name also searches a
/// relative folder such as `.`, where a cloned repository could hold its own `git` (#2948). With
/// no `git` in an absolute folder, the spawn fails as not found.
pub fn secure_git() -> Command {
    let path_var = std::env::var_os("PATH");
    let git = llmenv_paths::program::find_on_path("git", path_var.as_deref()).unwrap_or_else(|| {
        // The spawn error names no program, so the cause and the fix are logged here.
        tracing::error!(
            "git is not in any absolute PATH folder (relative folders such as `.` are skipped). \
             Fix: install git, or add its folder to PATH as an absolute path."
        );
        std::path::PathBuf::from(GIT_NOT_FOUND)
    });
    let mut cmd = Command::new(git);
    cmd.args(GIT_CONFIG_FLAGS);
    cmd.stdin(Stdio::null());
    cmd
}

/// Default TCP connection timeout for background git operations (fetch/pull on every shell
/// prompt). Short enough that a stuck remote doesn't freeze the prompt; long enough that a
/// briefly loaded GitHub doesn't produce spurious failures.
pub const DEFAULT_GIT_TIMEOUT_SECS: u64 = 10;

/// Default TCP connection timeout for explicit, user-initiated git operations (clone/fetch
/// for plugin installation). Longer than the background default because these are one-shot
/// operations and users understand that a clone can take a moment.
pub const DEFAULT_GIT_PLUGIN_TIMEOUT_SECS: u64 = 30;

/// Apply TCP connection and auth/transfer timeouts to a git command.
///
/// Sets three timeout mechanisms to prevent indefinite hangs (#449, #453):
/// - `GIT_CONNECT_TIMEOUT`: TCP handshake timeout
/// - `GIT_SSH_COMMAND`: SSH auth negotiation timeout via `-o ConnectTimeout` and
///   `-o BatchMode=yes` (non-interactive). Only sets this env var if not already present,
///   preserving any user-configured SSH identity files, ProxyJump, or other customizations.
/// - `http.lowSpeedTime` and `http.lowSpeedLimit`: HTTP pack transfer stall timeout
pub fn apply_git_timeout(cmd: &mut Command, secs: u64) -> &mut Command {
    cmd.env("GIT_CONNECT_TIMEOUT", secs.to_string());

    // Only set GIT_SSH_COMMAND if not already in the environment; preserve user's existing config
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env(
            "GIT_SSH_COMMAND",
            format!("ssh -o ConnectTimeout={secs} -o BatchMode=yes"),
        );
    }

    // HTTP transfer stall timeout: fail if download speed drops below 1 byte/sec for N seconds
    cmd.args([
        "-c",
        &format!("http.lowSpeedTime={secs}"),
        "-c",
        "http.lowSpeedLimit=1",
    ]);

    cmd
}

/// Scrub embedded credentials from a git URL before it lands in an error
/// message or log. A URL like `https://user:token@host/path` becomes
/// `https://***@host/path`; an SSH-style `user@host:path` becomes `***@host:path`.
/// Returns the input unchanged when no `@` userinfo is present.
#[must_use]
pub fn sanitize_git_url(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(proto_end) = url.find("://") {
            if at_pos > proto_end {
                let (proto, rest) = url.split_at(proto_end + 3);
                if let Some(host_start) = rest.find('@') {
                    return format!("{}***@{}", proto, &rest[host_start + 1..]);
                }
            }
        } else {
            return format!("***{}", &url[at_pos..]);
        }
    }
    url.to_string()
}

/// Build a human-readable failure detail from a git subprocess's captured
/// output. Prefers `stderr` (where git writes diagnostics), falls back to
/// `stdout` (some errors — e.g. a `git add` index lock — print there), then to
/// the exit status when both are empty. Control and ANSI escape bytes are
/// stripped so a hostile remote's error text can't manipulate the terminal
/// (#307), and the result is credential-scrubbed via [`sanitize_git_url`] so a
/// URL with embedded credentials never echoes the secret to the terminal or
/// logs (#312).
#[must_use]
pub fn git_failure_detail(
    stderr: &[u8],
    stdout: &[u8],
    status: std::process::ExitStatus,
) -> String {
    let raw = if stderr.is_empty() { stdout } else { stderr };
    let cleaned: String = String::from_utf8_lossy(raw)
        .trim()
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    if cleaned.is_empty() {
        format!("exit code {status}")
    } else {
        sanitize_git_url(&cleaned)
    }
}

/// Check if the working tree has staged or unstaged changes.
///
/// # Errors
/// Returns an error when git cannot start or exits non-zero. A failed read is not a clean tree.
pub fn working_tree_dirty(repo: &Path) -> Result<bool, String> {
    let output = secure_git()
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run git status in {}: {e}", repo.display()))?;
    if !output.status.success() {
        let detail = git_failure_detail(&output.stderr, &output.stdout, output.status);
        return Err(format!("git status failed in {}: {detail}", repo.display()));
    }
    Ok(!output.stdout.is_empty())
}

/// Check if current branch has commits not yet pushed to its upstream.
/// A branch with no upstream returns `Ok(false)`, because it has no push target to compare with.
///
/// # Errors
/// Returns an error when git cannot start, prints no count, or fails for any reason other than
/// a missing upstream. Those cases are not "nothing to push".
pub fn has_unpushed_commits(repo: &Path) -> Result<bool, String> {
    let output = secure_git()
        .args(["rev-list", "--count", "@{u}..HEAD"])
        .current_dir(repo)
        // Keeps git's message in English, so `is_missing_upstream` can match it.
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("cannot run git rev-list in {}: {e}", repo.display()))?;
    if !output.status.success() {
        let detail = git_failure_detail(&output.stderr, &output.stdout, output.status);
        // Only git's "no upstream configured" text means there is no push target. A gone
        // remote-tracking ref fails with other text, and that is an error, not "nothing to push".
        if detail.contains("no upstream configured") {
            return Ok(false);
        }
        return Err(format!(
            "git rev-list failed in {}: {detail}",
            repo.display()
        ));
    }
    parse_commit_count(&output.stdout)
        .map(|count| count > 0)
        .ok_or_else(|| format!("git rev-list printed no commit count in {}", repo.display()))
}

fn parse_commit_count(output: &[u8]) -> Option<u32> {
    std::str::from_utf8(output)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code, where a setup failure should panic"
)]
mod repo_state_tests;

#[cfg(test)]
mod tests;

/// How long a local git query may run, such as `rev-parse` or `symbolic-ref`. A local git answers
/// in milliseconds, so a longer wait means a stuck mount, and a hook must not wait on it (#2923).
const LOCAL_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run the local git query `args` in `dir` with the English messages that callers match on, for
/// at most [`LOCAL_QUERY_TIMEOUT`].
///
/// # Errors
/// git cannot start, or it runs past the timeout.
pub fn local_query(dir: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = secure_git();
    cmd.args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    llmenv_util::process::run_traced("git:local-query", cmd, LOCAL_QUERY_TIMEOUT)
}

/// Whether a failed git command ran in a folder outside a repository: git's fatal exit, 128, with
/// "not a git repository" in stderr. The phrase from any other exit, or from a kill by signal
/// (`None`), is a real failure (#3433).
///
/// Git may capitalise the message, so the match ignores case (#3040).
#[must_use]
pub fn is_outside_repo(code: Option<i32>, stderr: &str) -> bool {
    code == Some(128) && stderr.to_ascii_lowercase().contains("not a git repository")
}
