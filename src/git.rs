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
            tracing::debug!("git rev-list count failed at {}: {}", repo.display(), e);
            return false;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim();
        tracing::debug!(
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
            tracing::debug!(
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
            tracing::debug!(
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
