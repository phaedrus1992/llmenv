//! The bad-timestamp log: one warning per record and bad value, with each dedupe set bounded.

use super::{parse_rfc3339, validate_stored};
use anyhow::Context as _;
use std::time::SystemTime;

/// The id of the record that holds a stored timestamp, such as a task slug or a session id. It is
/// its own type, so a record id cannot take the place of the timestamp text (#3862).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecordId<'a>(pub(crate) &'a str);

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
pub(super) fn insert_once(
    set: &std::sync::Mutex<std::collections::BTreeSet<ReportKey>>,
    source: &str,
    record: &str,
    text: &str,
) -> bool {
    let mut guard = set
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.len() >= MAX_REPORTED {
        // A reset lets a value log again. The line lets a reader see why a value repeats (#3863).
        tracing::warn!(
            max = MAX_REPORTED,
            "the bad-timestamp dedupe set reached its cap and reset, so a repeated value may log \
             again"
        );
        guard.clear();
    }
    guard.insert((source.to_string(), record.to_string(), text.to_string()))
}

/// Whether `text` held by `record` from `source` is new to the bad-timestamp log.
pub(super) fn first_report(source: &'static str, record: RecordId<'_>, text: &str) -> bool {
    insert_once(&REPORTED_BAD, source, record.0, text)
}

/// Whether the winner note for `text` held by `record` from `source` is new.
pub(super) fn winner_report(source: &'static str, record: RecordId<'_>, text: &str) -> bool {
    insert_once(&REPORTED_WINNER, source, record.0, text)
}

/// The instant of a stored timestamp. `source` names the record kind in the warning, such as
/// `task created_at`, and `record` names the record, such as its slug. A value that does not
/// parse is logged once per record, and gives `None`.
pub(crate) fn instant_or_log(
    text: &str,
    source: &'static str,
    record: RecordId<'_>,
) -> Option<SystemTime> {
    match parse_rfc3339(text) {
        Ok(at) => Some(at),
        Err(e) => {
            if first_report(source, record, text) {
                tracing::warn!(
                    error = %e,
                    source,
                    record = record.0,
                    value = text,
                    "a stored timestamp cannot be read. Fix: rewrite it as RFC 3339, \
                     such as 2026-01-01T00:00:00Z"
                );
            }
            None
        }
    }
}

/// Check that the `field` of `record`, stored as `text`, is RFC 3339. A reader calls this at read
/// time, so a bad value fails there with the record and the fix, and not later in a sort (#3882).
///
/// # Errors
/// Errors if `text` is not RFC 3339. `record` names the record, such as `task 'x' at <path>`.
pub(crate) fn validate_stored_field(field: &str, text: &str, record: &str) -> anyhow::Result<()> {
    validate_stored(text).with_context(|| {
        format!(
            "{record} has an unreadable {field}. Fix: rewrite it as RFC 3339, such as \
             2026-01-01T00:00:00Z"
        )
    })
}

/// Warn when the record that wins a `max` holds a bad timestamp. A bad value sorts as the
/// oldest, so the record wins with no visible cue (#3843). `shared` counts the records that tie
/// with the winner by instant. When it is above 1, the winner is a tie, not the newest record.
/// Each record and value pair warns once per process, as `instant_or_log` does, because a status
/// line re-renders this often.
pub(crate) fn warn_if_bad_max_winner(
    source: &'static str,
    record: RecordId<'_>,
    text: &str,
    shared: usize,
) {
    let Err(e) = parse_rfc3339(text) else {
        return;
    };
    if winner_report(source, record, text) {
        tracing::warn!(
            error = %e,
            source,
            record = record.0,
            value = text,
            shared,
            "this record won the max with an unreadable timestamp, so the pick may be a tie. \
             Fix: rewrite the timestamp of this record as RFC 3339, such as 2026-01-01T00:00:00Z"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_REPORTED, insert_once};
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    proptest! {
        // Below the cap, a key answers "new" exactly once, whatever the order of the keys.
        #[test]
        fn insert_once_answers_new_once_per_key_below_the_cap(
            keys in proptest::collection::vec(("[ab]", "[xy]", "[0-9]{1,2}"), 1..40),
        ) {
            let set = Mutex::new(BTreeSet::new());
            let mut seen = BTreeSet::new();
            for (s, r, t) in &keys {
                let expected = seen.insert((s.clone(), r.clone(), t.clone()));
                prop_assert_eq!(insert_once(&set, s, r, t), expected);
            }
        }
    }

    #[test]
    fn the_reset_at_the_cap_makes_an_old_key_new_again_and_bounds_the_set() {
        let set = Mutex::new(BTreeSet::new());
        assert!(insert_once(&set, "s", "r", "first"));
        for i in 0..MAX_REPORTED {
            insert_once(&set, "s", "r", &format!("filler-{i}"));
        }
        let len = set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert!(
            len <= MAX_REPORTED,
            "the set holds {len} keys, above the cap"
        );
        assert!(
            insert_once(&set, "s", "r", "first"),
            "the first key was cleared by the reset, so it is new again"
        );
    }
}
