//! The bad-timestamp log: one warning per record and bad value, with each dedupe set bounded.

use super::parse_rfc3339;
use std::time::SystemTime;

/// One dedupe key: (source, record, text).
type ReportKey = (String, String, String);

/// Most keys one dedupe set holds. A process meets few bad values. Past this cap the set resets,
/// so a value may log again. A repeated line costs less than a set that grows without limit.
pub(super) const MAX_REPORTED: usize = 1024;

/// Bad timestamps that were already logged in this process. A sort compares one value many times,
/// so each bad value is logged once, not once per comparison (#3812). The source is part of the
/// key, so the same bad text from another record kind is still named. The record is part of the
/// key, so each record names itself once (#3851).
static REPORTED_BAD: std::sync::Mutex<std::collections::BTreeSet<ReportKey>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Records that already got the winner note. The note is a different message from the parse
/// warning, so it has its own set, and each message logs once per record.
static REPORTED_WINNER: std::sync::Mutex<std::collections::BTreeSet<ReportKey>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Insert a key into `set`, and return whether it was new. A poisoned lock still holds a valid
/// set, so the dedupe keeps working after a panic elsewhere.
fn insert_once(
    set: &std::sync::Mutex<std::collections::BTreeSet<ReportKey>>,
    source: &str,
    record: &str,
    text: &str,
) -> bool {
    let mut guard = set
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.len() >= MAX_REPORTED {
        guard.clear();
    }
    guard.insert((source.to_string(), record.to_string(), text.to_string()))
}

/// Whether `text` held by `record` from `source` is new to the bad-timestamp log.
pub(crate) fn first_report(source: &str, record: &str, text: &str) -> bool {
    insert_once(&REPORTED_BAD, source, record, text)
}

/// Whether the winner note for `text` held by `record` from `source` is new.
pub(super) fn winner_report(source: &str, record: &str, text: &str) -> bool {
    insert_once(&REPORTED_WINNER, source, record, text)
}

/// The instant of a stored timestamp. `source` names the record kind in the warning, such as
/// `task created_at`, and `record` names the record, such as its slug. A value that does not
/// parse is logged once per record, and gives `None`.
pub(crate) fn instant_or_log(text: &str, source: &str, record: &str) -> Option<SystemTime> {
    match parse_rfc3339(text) {
        Ok(at) => Some(at),
        Err(e) => {
            if first_report(source, record, text) {
                tracing::warn!(
                    error = %e,
                    source,
                    record,
                    value = text,
                    "a stored timestamp cannot be read. Fix: rewrite it as RFC 3339, \
                     such as 2026-01-01T00:00:00Z"
                );
            }
            None
        }
    }
}

/// Warn when the record that wins a `max` holds a bad timestamp. A bad value sorts as the
/// oldest, so the record wins with no visible cue (#3843). `shared` counts the records that tie
/// with the winner by instant. When it is above 1, the winner is a tie, not the newest record.
/// Each record and value pair warns once per process, as `instant_or_log` does, because a status
/// line re-renders this often.
pub(crate) fn warn_if_bad_max_winner(source: &str, record: &str, text: &str, shared: usize) {
    let Err(e) = parse_rfc3339(text) else {
        return;
    };
    if winner_report(source, record, text) {
        tracing::warn!(
            error = %e,
            source,
            record,
            value = text,
            shared,
            "this record won the max with an unreadable timestamp, so the pick may be a tie. \
             Fix: rewrite the timestamp of this record as RFC 3339, such as 2026-01-01T00:00:00Z"
        );
    }
}
