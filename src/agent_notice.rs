//! Notices for the agent, carried by hook output. The list lives in `llmenv_util`, so the
//! task crate and the hook code share one list.

pub(crate) use llmenv_util::agent_notice::{current, record};

/// Log `notice` at error level, which the default filter shows, and add it for the agent. The
/// engine drops a hook's stderr, so the log alone reaches no one.
pub(crate) fn record_logged(notice: impl Into<String>) {
    let notice = notice.into();
    tracing::error!("{notice}");
    record(notice);
}
