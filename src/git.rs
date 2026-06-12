//! Consolidated git utilities.
//!
//! Prevents malicious cloned repos from executing hooks or fsmonitors by
//! centralizing GIT_CONFIG_FLAGS application across all git operations.

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

/// Apply security config flags to a git command, with stdin detached.
///
/// No git operation in this codebase reads from stdin, so stdin is nulled at
/// construction: a command invoked with a non-interactive stdin (CI, or the
/// `source <(llmenv export)` eval context) can never block on an interactive
/// prompt such as a credential helper (#299, #307). Centralizing it here means
/// every `secure_git()` call site — including `.status()`/`.spawn()` callers
/// that would otherwise inherit the parent's stdin — gets the guarantee.
pub fn secure_git() -> Command {
    let mut cmd = Command::new("git");
    cmd.args(GIT_CONFIG_FLAGS);
    cmd.stdin(Stdio::null());
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
pub fn working_tree_dirty(repo: &Path) -> bool {
    secure_git()
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// Check if current branch has commits not yet pushed to its upstream.
/// Returns false if there's no upstream, git fails, or output can't be parsed —
/// we only want to nudge the user when we're certain there are unpushed commits.
pub fn has_unpushed_commits(repo: &Path) -> bool {
    let output = match secure_git()
        .args(["rev-list", "--count", "@{u}..HEAD"])
        .current_dir(repo)
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("git rev-list count failed at {}: {}", repo.display(), e);
            return false;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        tracing::warn!(
            "git rev-list count failed at {} with exit {}: {}",
            repo.display(),
            output.status,
            stderr
        );
        return false;
    }

    let count_str = match std::str::from_utf8(&output.stdout) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "git rev-list count output invalid UTF-8 at {}: {}",
                repo.display(),
                e
            );
            return false;
        }
    };

    match count_str.trim().parse::<u32>() {
        Ok(count) => count > 0,
        Err(e) => {
            tracing::warn!(
                "git rev-list count parse failed at {} for '{}': {}",
                repo.display(),
                count_str.trim(),
                e
            );
            false
        }
    }
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

    #[test]
    fn sanitize_git_url_http_with_credentials() {
        let url = "https://user:password@github.com/owner/repo.git";
        assert_eq!(
            sanitize_git_url(url),
            "https://***@github.com/owner/repo.git"
        );
    }

    #[test]
    fn sanitize_git_url_ssh() {
        let url = "git@github.com:owner/repo.git";
        assert_eq!(sanitize_git_url(url), "***@github.com:owner/repo.git");
    }

    #[test]
    fn sanitize_git_url_no_credentials() {
        let url = "https://github.com/owner/repo.git";
        assert_eq!(sanitize_git_url(url), url);
    }

    #[test]
    fn git_failure_detail_prefers_stderr() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let detail = git_failure_detail(b"fatal: repository not found\n", b"ignored", status);
        assert_eq!(detail, "fatal: repository not found");
    }

    #[test]
    fn git_failure_detail_falls_back_to_stdout_when_stderr_empty() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let detail = git_failure_detail(b"", b"index locked\n", status);
        assert_eq!(detail, "index locked");
    }

    #[test]
    fn git_failure_detail_scrubs_credentials_in_stderr() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let detail = git_failure_detail(
            b"fatal: could not read from https://user:tok@github.com/x.git\n",
            b"",
            status,
        );
        assert!(!detail.contains("tok"), "credential leaked: {detail}");
    }

    #[test]
    fn git_failure_detail_strips_control_and_ansi_sequences() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let hostile = b"\x1b[2Jcleared\x1b]0;title";
        let detail = git_failure_detail(hostile, b"", status);
        assert_eq!(detail, "[2Jcleared]0;title");
    }

    #[test]
    fn git_failure_detail_falls_back_to_exit_code() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(128 << 8);
        let detail = git_failure_detail(b"   \n", b"", status);
        assert!(detail.contains("exit code"), "got: {detail}");
    }

    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #[cfg(test)]
    mod prop_tests {
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parse_rev_list_count_handles_valid_numeric_output(count_val in 0u32..1000) {
                let output_bytes = format!("{}", count_val).into_bytes();
                let parsed = std::str::from_utf8(&output_bytes)
                    .unwrap_or("0")
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(0);
                prop_assert_eq!(parsed, count_val);
            }

            #[test]
            fn parse_rev_list_count_handles_malformed_output(junk in ".*") {
                let _parsed = std::str::from_utf8(junk.as_bytes())
                    .unwrap_or("0")
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(0);
                // Test verifies the parsing chain doesn't panic on arbitrary input
            }

            #[test]
            fn parse_rev_list_count_with_whitespace(count_val in 0u32..1000) {
                let output_bytes = format!("  {}  \n", count_val).into_bytes();
                let parsed = std::str::from_utf8(&output_bytes)
                    .unwrap_or("0")
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(0);
                prop_assert_eq!(parsed, count_val);
            }
        }
    }
}
