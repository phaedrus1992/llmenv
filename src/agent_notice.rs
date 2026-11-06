//! Notices for the agent, carried by hook output. The list lives in `llmenv_util`, so the
//! task crate and the hook code share one list.

#[cfg(test)]
pub(crate) use llmenv_util::agent_notice::forget_containing;
pub(crate) use llmenv_util::agent_notice::start_spool;
pub(crate) use llmenv_util::agent_notice::{current, record};

/// Log `notice` at error level, which the default filter shows, and add it for the agent. The
/// engine drops a hook's stderr, so the log alone reaches no one.
pub(crate) fn record_logged(notice: impl Into<String>) {
    let notice = notice.into();
    tracing::error!("{notice}");
    record(notice);
}

/// [`current`] after the notices that detached children spooled under the state folder are read
/// in, for the hook output (#3398). A state folder that cannot be found leaves the spool unread.
pub(crate) fn current_with_children() -> Option<String> {
    match crate::paths::state_dir() {
        Ok(state_dir) => llmenv_util::agent_notice::collect_spool(&state_dir),
        Err(e) => no_state_folder("the notices of detached jobs are not read", &e),
    }
    current()
}

/// Tell the agent that `skipped` did not happen because the state folder is missing or not
/// valid. The error says which.
pub(crate) fn no_state_folder(skipped: &str, error: &anyhow::Error) {
    record_logged(format!(
        "llmenv: {skipped}, because the state folder is not usable: {error:#}. Fix: set HOME, or \
         set LLMENV_STATE_DIR to a valid absolute path."
    ));
}

/// Record the top-level error of a detached child, so the next hook run shows it (#3398). Does
/// nothing in a process that does not spool, where `main` prints the error itself.
pub fn record_detached_job_failure(error: &anyhow::Error) {
    llmenv_util::agent_notice::record_detached_job_failure(error);
}
