//! Classifies a failed `gh pr view` run: no PR, a login problem, or a failure.

use serde::{Deserialize, Serialize};

use super::PrLookup;

/// A `gh pr view` call that gave no answer. A non-zero exit for "no open PR" or "no remote" is
/// an answer, not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::cli::statusline) enum GhFailure {
    /// `gh` is not installed.
    Missing,
    /// `gh` is not logged in, or its login expired.
    Unauthenticated,
    /// A timeout, a failed read or wait, or a spawn error other than a missing binary.
    Failed,
}

impl GhFailure {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Missing => "pr: gh missing",
            Self::Unauthenticated => "pr: gh not logged in",
            Self::Failed => "pr: gh failed",
        }
    }
}

/// The `gh pr view` errors that mean "there is no PR to show", not a failure. Entries are
/// lowercase and match case-insensitively: real `gh` capitalizes the first letter (#2975).
pub(super) const GH_NO_PR_SIGNS: [&str; 3] = [
    "no pull requests found",
    "no git remotes found",
    "not a git repository",
];

/// The exit code of `gh pr view` for each no-PR answer. Measured on gh 2.103.3: all three signs
/// exit 1, gh's code for a plain error. A no-PR sign with another exit, or a kill by a signal, is
/// a failure, because the text alone does not prove the answer (#3442).
const GH_NO_PR_EXIT: i32 = 1;

/// The `gh` errors that mean the login is missing or expired. Lowercase, like the no-PR list,
/// because the same case rule applies to both.
pub(super) const GH_LOGIN_SIGNS: [&str; 3] =
    ["gh auth login", "http 401", "authentication required"];

/// The lookup for a `gh pr view` that exited non-zero. Only a known "no PR" answer is
/// `Ok(None)`. The "no PR" check runs first, because that message quotes the branch name, and a
/// branch name can hold a login word. Any other exit, such as a network or rate-limit error, is
/// a failure, so it cannot look like "no open PR" (#2821).
pub(super) fn classify_gh_exit(status: std::process::ExitStatus, stderr: &str) -> PrLookup {
    // The same scrub as git's failure text strips control bytes and URL credentials before logging.
    let detail = llmenv_git::git_failure_detail(stderr.as_bytes(), &[], status);
    let first_line = detail.lines().next().unwrap_or("");
    let lowered = stderr.to_ascii_lowercase();
    if status.code() == Some(GH_NO_PR_EXIT) && GH_NO_PR_SIGNS.iter().any(|s| lowered.contains(s)) {
        tracing::debug!("gh pr view exited {status}, read as no PR: {first_line}");
        return Ok(None);
    }
    if GH_LOGIN_SIGNS.iter().any(|sign| lowered.contains(sign)) {
        tracing::error!("gh pr view: gh is not logged in ({status}): {first_line}");
        return Err(GhFailure::Unauthenticated);
    }
    tracing::error!("gh pr view failed ({status}): {first_line}");
    Err(GhFailure::Failed)
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    #[test]
    fn a_failed_gh_call_logs_its_stderr_without_credentials_or_escapes() {
        // An OSC sequence (ESC ]) is one the logger never writes, so its presence means the raw
        // stderr reached the log. The logger's own colors use ESC [ and do not count.
        let stderr = "error: https://user:hunter2@example.com/repo failed\u{1b}]0;title\u{7} red";
        let logs = crate::test_log_capture::capture_logs(|| {
            let _ = classify_gh_exit(exit_one(), stderr);
        });
        assert!(
            logs.contains("failed"),
            "the message was not logged: {logs}"
        );
        assert!(!logs.contains("hunter2"), "a credential was logged: {logs}");
        assert!(
            !logs.contains("\u{1b}]"),
            "an OSC escape was logged: {logs}"
        );
    }

    fn exit_one() -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(1 << 8)
    }

    /// Upper-case the first letter of each word: "no pull requests found" becomes
    /// "No Pull Requests Found". That is the kind of case real `gh` output can use.
    fn capitalized(sign: &str) -> String {
        sign.split(' ')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    proptest::proptest! {
        #[test]
        fn a_no_pr_sign_in_any_case_reads_as_no_pr(
            prefix in "[a-z ]{0,20}",
            suffix in "[a-z ]{0,20}",
            pick in 0..GH_NO_PR_SIGNS.len(),
        ) {
            let text = format!("{prefix}{}{suffix}", capitalized(GH_NO_PR_SIGNS[pick]));
            proptest::prop_assert_eq!(classify_gh_exit(exit_one(), &text), Ok(None));
        }

        /// A no-PR sign with any other exit, or with a kill by a signal, is a failure (#3442).
        #[test]
        fn a_no_pr_sign_with_another_exit_is_a_failure(
            pick in 0..GH_NO_PR_SIGNS.len(),
            code in 2i32..256,
            signal in 1i32..32,
        ) {
            let text = GH_NO_PR_SIGNS[pick];
            let by_code = std::process::ExitStatus::from_raw(code << 8);
            proptest::prop_assert_eq!(classify_gh_exit(by_code, text), Err(GhFailure::Failed));
            let by_signal = std::process::ExitStatus::from_raw(signal);
            proptest::prop_assert_eq!(classify_gh_exit(by_signal, text), Err(GhFailure::Failed));
        }

        #[test]
        fn a_login_sign_in_any_case_reads_as_unauthenticated(
            prefix in "[a-z ]{0,20}",
            pick in 0..GH_LOGIN_SIGNS.len(),
        ) {
            let text = format!("{prefix}{}", capitalized(GH_LOGIN_SIGNS[pick]));
            proptest::prop_assert_eq!(
                classify_gh_exit(exit_one(), &text),
                Err(GhFailure::Unauthenticated)
            );
        }
    }
}
