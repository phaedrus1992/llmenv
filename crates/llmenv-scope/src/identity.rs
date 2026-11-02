//! Host, user, and working-directory probes for scope matching.
//!
//! A failed probe switches off one scope kind. A hook's stderr never reaches the agent, so each
//! failure is also recorded as an agent notice (#3224).

/// The kernel nodename, or `None` when it is empty.
pub(super) fn detect_hostname() -> Option<String> {
    // uname(2) syscall rather than spawning the `hostname` binary: each hook-run
    // is a fresh process, so the process-static Env cache never helps the hook
    // path — the subprocess fork/exec ran on every hook and dominated scope
    // evaluation (~15ms/event). rustix reads the kernel nodename with one
    // syscall, no subprocess, no unsafe.
    let nodename = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let trimmed = nodename.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// The hostname for host-scope matching. An empty string disables host matching.
/// A silently disabled scope changes which bundles load (#2666), so the failure is recorded.
pub(super) fn hostname_or_notice(hostname: Option<String>) -> String {
    hostname.unwrap_or_else(|| {
        llmenv_util::agent_notice::record_logged(
            "llmenv: the system hostname is empty; host-scope matching is off. \
             Fix: set the system hostname, then start a new session.",
        );
        String::new()
    })
}

/// The user for user-scope matching. An empty string disables user matching.
pub(super) fn user_or_notice(user: Result<String, std::env::VarError>) -> String {
    match user {
        Ok(user) if !user.is_empty() => user,
        Ok(_) | Err(std::env::VarError::NotPresent) => {
            llmenv_util::agent_notice::record_logged(
                "llmenv: $USER is unset or empty; user-scope matching is off. \
                 Fix: set USER in the shell environment, then start a new session.",
            );
            String::new()
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            llmenv_util::agent_notice::record_logged(
                "llmenv: $USER is not valid UTF-8; user-scope matching is off. \
                 Fix: set USER to a UTF-8 user name, then start a new session.",
            );
            String::new()
        }
    }
}

/// The working directory for project matching. An empty string disables project matching.
pub(super) fn cwd_or_notice(cwd: Option<String>) -> String {
    cwd.unwrap_or_else(|| {
        llmenv_util::agent_notice::record_logged(
            "llmenv: current directory is unavailable or not UTF-8; project-scope matching \
             is off. Fix: start the session in an existing directory with a UTF-8 path.",
        );
        String::new()
    })
}

#[cfg(test)]
mod tests {
    use super::{cwd_or_notice, hostname_or_notice, user_or_notice};

    #[test]
    fn missing_hostname_is_recorded_for_the_agent() {
        assert!(hostname_or_notice(None).is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("hostname is empty"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    #[test]
    fn unset_user_is_recorded_for_the_agent() {
        assert!(user_or_notice(Err(std::env::VarError::NotPresent)).is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("$USER is unset"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    #[test]
    fn unreadable_user_is_recorded_for_the_agent() {
        let failure = std::env::VarError::NotUnicode(std::ffi::OsString::from("x"));
        assert!(user_or_notice(Err(failure)).is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("USER is not valid UTF-8"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    #[test]
    fn missing_cwd_is_recorded_for_the_agent() {
        assert!(cwd_or_notice(None).is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("current directory is unavailable"),
            "{notices}"
        );
        assert!(notices.contains("Fix:"), "{notices}");
    }
}

#[cfg(test)]
mod empty_user_tests {
    use super::user_or_notice;

    #[test]
    fn an_empty_user_is_recorded_for_the_agent() {
        assert!(user_or_notice(Ok(String::new())).is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("unset or empty"), "{notices}");
    }
}
