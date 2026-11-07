//! Notices for the agent, carried by hook output.
//!
//! The engine drops a hook's stderr, so a log line reaches no one. A failure that the agent must
//! see is recorded here, and the hook output carries every notice of its process (#2658).

use std::sync::{Mutex, PoisonError};

mod spool;
pub(crate) use spool::start as start_spool;

/// The most characters of one notice. A notice can carry an error chain, so the cap keeps one
/// failure from filling the agent's context.
const MAX_NOTICE_CHARS: usize = 1000;

/// Marks the next step in a notice. Every notice that names a fix ends with this marker, so the
/// cap keeps it (#2820).
const FIX_MARKER: &str = "Fix:";

/// One line, free of control and bidirectional characters, and at most [`MAX_NOTICE_CHARS`].
/// When the text is too long, the cause is cut, and a `…` marks the cut. The `Fix:` part is kept
/// whole. The last `Fix:` marks the fix, so a cause that repeats the marker does not move it.
fn clean(text: &str) -> String {
    let spaced = text.replace(['\n', '\r'], " ");
    let plain = crate::util::strip_unsafe_chars(&spaced);
    if plain.chars().count() <= MAX_NOTICE_CHARS {
        return plain;
    }
    match plain.rfind(FIX_MARKER) {
        // Room for the fix, one separating space, and at least the marker and one cause character.
        Some(at) if plain[at..].chars().count() + 2 <= MAX_NOTICE_CHARS => {
            let fix = &plain[at..];
            let cause = plain[..at].trim_end();
            let budget = MAX_NOTICE_CHARS - fix.chars().count() - 1;
            if cause.chars().count() <= budget {
                return format!("{cause} {fix}");
            }
            let kept: String = cause.chars().take(budget - 1).collect();
            format!("{}… {fix}", kept.trim_end())
        }
        _ => {
            let kept: String = plain.chars().take(MAX_NOTICE_CHARS - 1).collect();
            format!("{kept}…")
        }
    }
}

/// The notices of one process, in the order they were first raised.
#[derive(Debug)]
pub(crate) struct NoticeList {
    notices: Vec<String>,
}

impl NoticeList {
    /// An empty list. `const` so the process-wide list can be a `static`.
    const fn new() -> Self {
        Self {
            notices: Vec::new(),
        }
    }

    /// Add a notice. A notice that is already present is not added again, so a failure that
    /// repeats does not repeat its text. The text is cleaned first: control and bidirectional
    /// characters are removed, line breaks become spaces, and the text is capped, because a
    /// failure text can carry server or URL content and `current` joins notices with newlines.
    fn record(&mut self, notice: impl Into<String>) -> bool {
        let notice = clean(&notice.into());
        if self.notices.contains(&notice) {
            return false;
        }
        self.notices.push(notice);
        true
    }

    /// Every notice, one per line, or `None` when there is none.
    fn current(&self) -> Option<String> {
        (!self.notices.is_empty()).then(|| self.notices.join("\n"))
    }
}

/// Notices raised in this process.
static NOTICES: Mutex<NoticeList> = Mutex::new(NoticeList::new());

/// Add a notice for the agent. See `NoticeList::record`. A detached child that spools also writes
/// the notice to the spool, so the next hook run carries it (#3398).
pub(crate) fn record(notice: impl Into<String>) {
    let notice = clean(&notice.into());
    // A repeat of a notice in this process is not written to the spool again, so it does not use
    // up the reads of the next hook run (#3507).
    let fresh = NOTICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(notice.clone());
    if fresh {
        spool::write(&notice);
    }
}

/// Log `notice` at error level, which the default filter shows, and add it for the agent. The
/// engine drops a hook's stderr, so the log alone reaches no one.
pub(crate) fn record_logged(notice: impl Into<String>) {
    let notice = notice.into();
    tracing::error!("{notice}");
    record(notice);
}

/// Every notice of this process, one per line, or `None` when there is none. The list stays set,
/// so every call returns the same notices.
pub(crate) fn current() -> Option<String> {
    NOTICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .current()
}

/// [`current`] after the notices that detached children spooled under the state folder are read
/// in, for the hook output (#3398). A state folder that cannot be found leaves the spool unread.
pub(crate) fn current_with_children() -> Option<String> {
    match crate::paths::state_dir() {
        Ok(state_dir) => spool::collect(&state_dir),
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

/// Record a fixed notice for an error whose chain can quote other text, such as a backend reply or
/// a project's memory. The chain goes to the log, and the agent gets only `notice`, so the spool
/// never carries that text to another project (#3507).
pub(crate) fn record_fixed_with_cause(notice: &str, cause: &anyhow::Error) {
    tracing::error!(error = %format!("{cause:#}"), "{}", notice);
    record(notice);
}

/// The agent notice for a failed detached child. It holds no error text, because the cause is in the
/// log (#3507).
pub(crate) const DETACHED_JOB_NOTICE: &str = "llmenv: a detached job failed. Fix: run `llmenv \
     doctor` to check the state folder and the config. The cause is in the llmenv log.";

/// Record that a detached child failed, so the next hook run shows it (#3398). The agent gets a
/// fixed text, because the spool is read by whichever project runs the next hook, and the error
/// chain can quote another project's text (#3507). The chain goes to the log. Does nothing in a
/// process that does not spool, where `main` prints the error itself.
pub fn record_detached_job_failure(error: &anyhow::Error) {
    tracing::error!(error = %format!("{error:#}"), "a detached job failed");
    // A job that already noticed its own failure is not noticed twice.
    if spool::active() && !JOB_NOTICED.load(std::sync::atomic::Ordering::SeqCst) {
        record(DETACHED_JOB_NOTICE);
    }
}

/// Set once a detached job has recorded its own specific notice in this process.
static JOB_NOTICED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record a job's own fixed notice for its failure, so `record_detached_job_failure` adds none.
pub(crate) fn record_job_notice_with_cause(notice: &str, cause: &anyhow::Error) {
    JOB_NOTICED.store(true, std::sync::atomic::Ordering::SeqCst);
    record_fixed_with_cause(notice, cause);
}

/// Drop the notices that contain `text`, so a test sees only the notices of its own run.
#[cfg(test)]
pub(crate) fn forget_containing(text: &str) {
    NOTICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .notices
        .retain(|notice| !notice.contains(text));
}

#[cfg(test)]
mod tests {
    use super::{NoticeList, clean, current, record};
    use proptest::prelude::*;

    #[test]
    fn a_repeated_notice_is_kept_once() {
        record("agent_notice test: repeated line");
        record("agent_notice test: repeated line");
        let text = current().unwrap_or_default();
        assert_eq!(text.matches("agent_notice test: repeated line").count(), 1);
    }

    #[test]
    fn notices_keep_the_order_they_were_raised_in() {
        record("agent_notice test: first line");
        record("agent_notice test: second line");
        let text = current().unwrap_or_default();
        let first = text.find("agent_notice test: first line");
        let second = text.find("agent_notice test: second line");
        assert!(
            first.is_some() && second.is_some(),
            "both notices must be present: {text}"
        );
        assert!(
            first < second,
            "the first raised notice must come first: {text}"
        );
    }

    #[test]
    fn a_long_cause_keeps_its_fix_line_whole() {
        let cause = "e".repeat(2000);
        let fix = "Fix: run `llmenv doctor` to check the state directory.";
        let mut list = NoticeList::new();
        list.record(format!("llmenv: a step failed: {cause}. {fix}"));
        let text = list.current().unwrap_or_default();
        assert_eq!(text.chars().count(), 1000);
        assert!(text.ends_with(fix), "the fix line was cut: {text}");
        assert!(
            text.starts_with("llmenv: a step failed: e"),
            "the cause head was lost: {text}"
        );
    }

    #[test]
    fn a_long_notice_without_a_fix_line_is_cut_from_the_end() {
        let mut list = NoticeList::new();
        list.record("x".repeat(2000));
        assert_eq!(list.current().map(|t| t.chars().count()), Some(1000));
    }

    #[test]
    fn a_fix_line_longer_than_the_cap_is_cut_plainly() {
        let mut list = NoticeList::new();
        list.record(format!("cause. Fix: {}", "f".repeat(1500)));
        let text = list.current().unwrap_or_default();
        assert_eq!(text.chars().count(), 1000);
        assert!(text.starts_with("cause. Fix: f"));
    }

    #[test]
    fn a_capped_notice_stays_in_the_cap_and_keeps_its_fix_line() {
        const FIX: &str = "Fix: run `llmenv doctor` to check the state directory.";
        proptest!(|(cause in "(?s).{0,2500}")| {
            let text = clean(&format!("llmenv: a step failed: {cause}. {FIX}"));
            prop_assert!(text.chars().count() <= 1000, "over the cap: {}", text.chars().count());
            prop_assert!(text.ends_with(FIX), "fix line lost: {text}");
        });
    }

    #[test]
    fn a_notice_with_a_line_break_stays_one_line() {
        let mut list = NoticeList::new();
        list.record("first\nsecond\r\nthird");
        assert_eq!(list.current().as_deref(), Some("first second  third"));
    }

    proptest! {
        #[test]
        fn every_line_is_clean_and_the_line_count_is_the_distinct_count(
            raised in proptest::collection::vec("[a-z \n\r\u{202e}\u{200b}\u{7}]{0,12}", 0..16),
        ) {
            let mut list = NoticeList::new();
            for notice in &raised {
                list.record(notice.as_str());
            }
            let text = list.current().unwrap_or_default();
            for line in text.lines() {
                prop_assert!(
                    !line.chars().any(|c| c.is_control() || c == '\u{202e}' || c == '\u{200b}'),
                    "unclean line: {line:?}"
                );
            }
            let distinct: std::collections::BTreeSet<&str> = text.lines().collect();
            prop_assert_eq!(distinct.len(), text.lines().count());
        }

        #[test]
        fn recording_a_notice_twice_changes_nothing(notice in "[ -~]{0,40}") {
            let mut once = NoticeList::new();
            once.record(notice.as_str());
            let mut twice = NoticeList::new();
            twice.record(notice.as_str());
            twice.record(notice.as_str());
            prop_assert_eq!(once.current(), twice.current());
        }

        #[test]
        fn current_is_the_first_occurrence_dedupe_joined_by_newlines(
            raised in proptest::collection::vec("[a-z \n\r]{0,12}", 0..16),
        ) {
            let mut list = NoticeList::new();
            let mut expected: Vec<String> = Vec::new();
            for notice in &raised {
                list.record(notice.as_str());
                let flat = notice.replace(['\n', '\r'], " ");
                if !expected.contains(&flat) {
                    expected.push(flat);
                }
            }
            let want = (!expected.is_empty()).then(|| expected.join("\n"));
            prop_assert_eq!(list.current(), want);
        }
    }
}
