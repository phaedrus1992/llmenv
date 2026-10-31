//! The last words of a stopped MCP server, read from its stderr.

use std::time::Duration;

/// What the stopped server left on stderr, as one clean line. A read that fails or times out is
/// recorded for the agent, because the server's last words are lost with it (#3217).
pub(super) async fn stderr_tail(
    stderr: Option<&mut tokio::process::ChildStderr>,
    command: &str,
) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let stderr = stderr?;
    let mut buf = Vec::new();
    // The child is stopped, so the pipe ends at its last write. The timeout is a backstop for a
    // grandchild that still holds the pipe open.
    let wait = Duration::from_millis(200);
    match tokio::time::timeout(wait, (&mut *stderr).take(2048).read_to_end(&mut buf)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => llmenv_util::agent_notice::record_logged(format!(
            "llmenv: cannot read the stderr of `{command}` ({e}), so its last words are missing. \
             Fix: run `{command}` by hand and check what it writes to stderr."
        )),
        Err(_) => llmenv_util::agent_notice::record_logged(format!(
            "llmenv: the stderr of `{command}` did not close within {} ms, so its last words may be \
             cut. Fix: stop any process that `{command}` started, then run the check again.",
            wait.as_millis()
        )),
    }
    let text = crate::stdio_rpc::tidy_reason(&String::from_utf8_lossy(&buf));
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod notice_tests {
    use super::*;

    /// A server that keeps its stderr open is cut off by the timeout, and the cut is named for
    /// the agent. The `sleep` child holds the pipe open, as a grandchild would (#3217).
    #[tokio::test]
    async fn a_timed_out_stderr_read_is_recorded_for_the_agent() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("5")
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stderr = child.stderr.take();
        assert_eq!(
            stderr_tail(stderr.as_mut(), "sleep-3217-holds-pipe").await,
            None
        );
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("sleep-3217-holds-pipe"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }
}
