//! Timestamps and durations: RFC 3339 text, age text, and the `--older-than` value. All of it
//! goes through `jiff`, so no other crate is needed for time text (#3790).

use jiff::{SignedDuration, Span, SpanRelativeTo, SpanRound, Timestamp, Unit, fmt::friendly};
use std::time::{Duration, SystemTime};

mod bad_log;
pub use bad_log::{RecordId, instant_or_log, validate_stored_field, warn_if_bad_max_winner};

/// The format of a stored timestamp. It always has 9 fractional digits, so the text of two
/// timestamps sorts in time order.
const RFC3339_NANOS: &str = "%Y-%m-%dT%H:%M:%S%.9fZ";

/// The current time as RFC 3339 text in UTC, with nanoseconds.
#[must_use]
pub fn now_rfc3339_nanos() -> String {
    format_nanos(Timestamp::now())
}

fn format_nanos(ts: Timestamp) -> String {
    ts.strftime(RFC3339_NANOS).to_string()
}

/// Parse RFC 3339 text. An offset other than `Z` is converted to the same instant.
///
/// # Errors
/// Errors if `text` is not RFC 3339.
pub fn parse_rfc3339(text: &str) -> anyhow::Result<SystemTime> {
    let ts: Timestamp = text
        .parse()
        .map_err(|e| anyhow::anyhow!("{text:?} is not an RFC 3339 timestamp: {e}"))?;
    Ok(SystemTime::from(ts))
}

/// Check that a stored timestamp parses. A reader calls this before the value reaches a sort, so a
/// bad value fails at the read and not in the order (#3856).
///
/// # Errors
/// Errors if `text` is not RFC 3339.
pub fn validate_stored(text: &str) -> anyhow::Result<()> {
    parse_rfc3339(text).map(|_| ())
}

/// Orders two stored timestamps by instant. The text order is wrong for some values: in text,
/// `Z` sorts after `.`, so a whole second reads later than a fraction of it (#3796). A value that
/// does not parse sorts first. Its warning is the only signal, and the warning is logged once per
/// record. `a_record` and `b_record` name the records that hold the two values in that log.
#[must_use]
pub fn cmp_instants(
    a: &str,
    a_record: RecordId<'_>,
    b: &str,
    b_record: RecordId<'_>,
    source: &'static str,
) -> std::cmp::Ordering {
    instant_or_log(a, source, a_record).cmp(&instant_or_log(b, source, b_record))
}

/// `age` as compact text, such as `"1d 2h 3m 4s"`. A zero age is `"0s"`. Days are 24 hours.
#[must_use]
pub fn format_age(age: Duration) -> String {
    let printer = friendly::SpanPrinter::new().designator(friendly::Designator::Compact);
    let Ok(signed) = SignedDuration::try_from(age) else {
        // Over 292 billion years: the text is the seconds, so it still ends in a unit.
        return format!("{}s", age.as_secs());
    };
    let balanced = Span::try_from(signed).and_then(|span| {
        span.round(
            SpanRound::new()
                .largest(Unit::Day)
                .relative(SpanRelativeTo::days_are_24_hours()),
        )
    });
    match balanced {
        Ok(span) => printer.span_to_string(&span),
        Err(e) => {
            // The plain form is still a correct age, so the rounding failure is only a debug note.
            tracing::debug!(error = %e, "could not round the age, so it uses the plain form");
            printer.duration_to_string(&signed)
        }
    }
}

/// Parse a duration such as `14d`, `1w`, `90 days`, or `1h30m`. A week is 7 days, and a day is
/// 24 hours.
///
/// # Errors
/// Errors if `raw` is not a duration, is negative, or uses months or years, which have no fixed
/// length. The message says how to fix the value.
pub fn parse_age(raw: &str) -> anyhow::Result<Duration> {
    let span: Span = raw.parse().map_err(|e| {
        anyhow::anyhow!(
            "cannot read the duration {raw:?}: {e}. Give a number and a unit, such as 14d or 1w"
        )
    })?;
    // Only months and years have no fixed length. A value that is too large fails in the parse
    // above, with the range of the unit (#3860).
    if span.get_years() != 0 || span.get_months() != 0 {
        anyhow::bail!(
            "the duration {raw:?} uses months or years, which have no fixed length. \
             Give it in weeks or days, such as 52w or 365d"
        );
    }
    let signed = span
        .to_duration(SpanRelativeTo::days_are_24_hours())
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot convert the duration {raw:?} to seconds: {e}. Give a smaller value"
            )
        })?;
    Duration::try_from(signed)
        .map_err(|_| anyhow::anyhow!("the duration {raw:?} is negative. Give a positive duration"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use super::bad_log::{MAX_REPORTED, first_report, insert_once, winner_report};
    use super::*;
    use proptest::prelude::*;

    /// Stored text as a sort sees it: a valid instant, or any other text.
    fn stored_text() -> impl Strategy<Value = String> {
        prop_oneof![
            "\\PC{0,30}",
            (0i64..4_000_000_000)
                .prop_map(|s| format_nanos(Timestamp::from_second(s).expect("in range"))),
        ]
    }

    proptest! {
        // A sort needs a total preorder: swapping the arguments reverses the order, and the order
        // is transitive over mixed valid and bad text.
        #[test]
        fn cmp_instants_is_a_total_preorder(a in stored_text(), b in stored_text(), c in stored_text()) {
            prop_assert_eq!(
                cmp_instants(&a, RecordId("ra"), &b, RecordId("rb"), "test"),
                cmp_instants(&b, RecordId("rb"), &a, RecordId("ra"), "test").reverse()
            );
            if cmp_instants(&a, RecordId("ra"), &b, RecordId("rb"), "test").is_le()
                && cmp_instants(&b, RecordId("rb"), &c, RecordId("rc"), "test").is_le()
            {
                prop_assert!(cmp_instants(&a, RecordId("ra"), &c, RecordId("rc"), "test").is_le());
            }
        }

        // Each distinct (source, record, bad value) triple is reported once. The set is local to
        // the case, so another test cannot reset it between two inserts (#3812).
        #[test]
        fn first_report_is_true_once_per_distinct_source_record_and_text(
            triples in proptest::collection::vec(("(a|b)", "(r|s)", "[a-z]{1,4}"), 1..12),
        ) {
            let set = std::sync::Mutex::new(std::collections::BTreeSet::new());
            let keys: Vec<(String, String, String)> = triples
                .iter()
                .map(|(s, r, t)| {
                    (format!("pbt-src-{s}"), format!("pbt-rec-{r}"), format!("pbt-3812-{t}"))
                })
                .collect();
            let distinct = keys.iter().collect::<std::collections::BTreeSet<_>>().len();
            let firsts = keys
                .iter()
                .filter(|(s, r, t)| insert_once(&set, s, r, t))
                .count();
            prop_assert_eq!(firsts, distinct);
        }
    }

    #[test]
    fn a_bad_value_is_reported_once_per_process() {
        let bad = "not-a-time-3812-unique";
        assert!(
            first_report("test-3812", RecordId("rec"), bad),
            "the first report of a bad value is logged"
        );
        assert!(
            !first_report("test-3812", RecordId("rec"), bad),
            "a repeat of the same bad value is not logged again"
        );
    }

    #[test]
    fn the_winner_note_and_the_parse_warning_each_log_once_per_record() {
        let bad = "not-a-time-3863-unique";
        assert!(
            first_report("test-3863", RecordId("rec"), bad),
            "parse warning is new"
        );
        assert!(
            winner_report("test-3863", RecordId("rec"), bad),
            "winner note is new"
        );
        assert!(
            !first_report("test-3863", RecordId("rec"), bad),
            "parse warning repeats"
        );
        assert!(
            !winner_report("test-3863", RecordId("rec"), bad),
            "winner note repeats"
        );
    }

    #[test]
    fn the_dedupe_set_resets_past_its_cap_so_an_evicted_value_logs_again() {
        // The first key is cleared by a reset, because every insert after it pushes the set past
        // the cap. So the reset has to run before the last insert, whatever other tests add.
        let first = ("test-3863-cap", "r", "first");
        assert!(first_report(first.0, RecordId(first.1), first.2));
        for i in 0..=MAX_REPORTED {
            first_report("test-3863-cap", RecordId("r"), &format!("filler-{i}"));
        }
        assert!(
            first_report(first.0, RecordId(first.1), first.2),
            "the key was evicted, so it logs again"
        );
    }

    #[test]
    fn a_bad_value_is_reported_once_per_source() {
        // The same bad text in another record kind is a new report, so its source is named too.
        let bad = "not-a-time-3834-unique";
        assert!(first_report("task created_at", RecordId("rec"), bad));
        assert!(first_report("session started_at", RecordId("rec"), bad));
        assert!(!first_report("task created_at", RecordId("rec"), bad));
    }

    #[test]
    fn a_bad_value_is_reported_once_per_record() {
        // The same bad text in two records of one kind names each record once (#3851).
        let bad = "not-a-time-3851-unique";
        assert!(first_report("task created_at", RecordId("slug-one"), bad));
        assert!(first_report("task created_at", RecordId("slug-two"), bad));
        assert!(!first_report("task created_at", RecordId("slug-one"), bad));
    }

    #[test]
    fn cmp_instants_orders_by_instant_not_by_text() {
        // In text, 'Z' sorts after '.', so the whole second reads as later. It is earlier.
        let whole = "2026-01-01T00:00:01Z";
        let half = "2026-01-01T00:00:01.500000000Z";
        assert_eq!(
            cmp_instants(whole, RecordId("a"), half, RecordId("b"), "test"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            cmp_instants(half, RecordId("b"), whole, RecordId("a"), "test"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn cmp_instants_sorts_an_unparseable_value_first() {
        let good = "2026-01-01T00:00:00Z";
        assert_eq!(
            cmp_instants("garbage", RecordId("a"), good, RecordId("b"), "test"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            cmp_instants(good, RecordId("b"), "garbage", RecordId("a"), "test"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            cmp_instants("garbage", RecordId("a"), "other", RecordId("b"), "test"),
            std::cmp::Ordering::Equal
        );
    }

    proptest::proptest! {
        // The helper has an instant exactly when the strict parser accepts the text, and it never
        // panics on any input.
        #[test]
        fn instant_or_log_is_some_exactly_when_the_text_parses(text in "\\PC{0,40}") {
            proptest::prop_assert_eq!(
                instant_or_log(&text, "test", RecordId("rec")).is_some(),
                parse_rfc3339(&text).is_ok()
            );
            // The read-time check and the sort-time parse must agree on every value.
            proptest::prop_assert_eq!(
                validate_stored(&text).is_ok(),
                instant_or_log(&text, "test", RecordId("rec")).is_some()
            );
        }

        // Two instants compare by instant, even when one is written in the minimal form and the
        // other in the fixed 9-digit form (#3796).
        #[test]
        fn cmp_instants_compares_mixed_precision_texts_by_instant(
            secs in 0i64..4_000_000_000,
            a_nanos in 0i32..1_000_000_000,
            a_digits in 0u32..=9,
            b_nanos in 0i32..1_000_000_000,
        ) {
            // Drop the digits past `a_digits`, so the minimal text of `a` has fewer digits than
            // the fixed 9-digit text of `b`. Random nanoseconds alone rarely produce that mix.
            let unit = 10i32.pow(9 - a_digits);
            let a = Timestamp::new(secs, a_nanos / unit * unit).expect("in range");
            let b = Timestamp::new(secs, b_nanos).expect("in range");
            proptest::prop_assert_eq!(
                cmp_instants(&a.to_string(), RecordId("a"), &format_nanos(b), RecordId("b"), "test"),
                a.cmp(&b)
            );
        }
    }

    #[test]
    fn age_text_uses_compact_units_and_days() {
        assert_eq!(format_age(Duration::from_secs(0)), "0s");
        assert_eq!(format_age(Duration::from_secs(61)), "1m 1s");
        assert_eq!(format_age(Duration::from_secs(7300)), "2h 1m 40s");
        assert_eq!(format_age(Duration::from_secs(93_784)), "1d 2h 3m 4s");
    }

    #[test]
    fn a_duration_reads_in_days_weeks_and_words() {
        let day = 86_400;
        assert_eq!(
            parse_age("14d").expect("14d"),
            Duration::from_secs(14 * day)
        );
        assert_eq!(parse_age("1w").expect("1w"), Duration::from_secs(7 * day));
        assert_eq!(
            parse_age("90 days").expect("90 days"),
            Duration::from_secs(90 * day)
        );
        assert_eq!(
            parse_age("1h30m").expect("1h30m"),
            Duration::from_secs(5400)
        );
    }

    #[test]
    fn a_bad_duration_names_the_value_and_the_fix() {
        for bad in ["10", "", "soon", "-1d"] {
            let err = parse_age(bad).expect_err(bad).to_string();
            assert!(err.contains(&format!("{bad:?}")), "{err}");
        }
        for calendar in ["1y", "1mo", "1y2d", "10000y"] {
            let err = parse_age(calendar).expect_err(calendar).to_string();
            assert!(err.contains("52w or 365d"), "{err}");
        }
    }

    /// #3860: a day count past the range of the unit is not a calendar unit, and the message
    /// says why.
    #[test]
    fn a_duration_too_large_for_its_unit_is_not_called_a_calendar_unit() {
        for big in ["999999999999999d", "99999999999w", "7304485d"] {
            let err = parse_age(big).expect_err(big).to_string();
            assert!(err.contains(&format!("{big:?}")), "{err}");
            assert!(err.contains("not in the required range"), "{err}");
            assert!(!err.contains("months or years"), "{err}");
        }
        assert!(parse_age("7304484d").is_ok());
    }

    #[test]
    fn rfc3339_text_round_trips_with_nine_digits_and_sorts() {
        let early = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 5_000_000);
        let late = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 50_000_000);
        let a = format_nanos(Timestamp::try_from(early).expect("in range"));
        let b = format_nanos(Timestamp::try_from(late).expect("in range"));
        assert_eq!(a, "2023-11-14T22:13:20.005000000Z");
        assert!(a < b, "text order must follow time order");
        assert_eq!(parse_rfc3339(&a).expect("parse"), early);
    }

    #[test]
    fn an_offset_is_converted_to_the_same_instant() {
        let z = parse_rfc3339("2026-11-17T14:19:20Z").expect("z");
        let plus = parse_rfc3339("2026-11-17T16:19:20+02:00").expect("offset");
        assert_eq!(z, plus);
        assert!(parse_rfc3339("garbage").is_err());
    }

    proptest! {
        /// Stored text reads back to the same instant, and text order follows time order.
        #[test]
        fn rfc3339_text_round_trips_and_sorts(
            a in 0i64..4_000_000_000, an in 0u32..1_000_000_000,
            b in 0i64..4_000_000_000, bn in 0u32..1_000_000_000,
        ) {
            let at = |s: i64, n: u32| Timestamp::new(s, i32::try_from(n).expect("nanos")).expect("in range");
            let (ta, tb) = (at(a, an), at(b, bn));
            let (xa, xb) = (format_nanos(ta), format_nanos(tb));
            prop_assert_eq!(parse_rfc3339(&xa).expect("parse"), SystemTime::from(ta));
            prop_assert_eq!(ta.cmp(&tb), xa.cmp(&xb));
        }

        /// Years and months are refused with the calendar text. Days and weeks read back as that
        /// many seconds, up to the range of the unit (#3860).
        #[test]
        fn parse_age_refuses_calendar_units_and_reads_fixed_units_back(
            years in 1u32..10_000,
            days in 0u64..7_304_485,
            weeks in 0u64..1_043_498,
        ) {
            for text in [format!("{years}y"), format!("{years}mo")] {
                let err = parse_age(&text).expect_err("a calendar unit is refused").to_string();
                prop_assert!(err.contains("months or years"), "{err}");
            }
            prop_assert_eq!(
                parse_age(&format!("{days}d")).expect("days read back"),
                Duration::from_secs(days * 86_400)
            );
            prop_assert_eq!(
                parse_age(&format!("{weeks}w")).expect("weeks read back"),
                Duration::from_secs(weeks * 7 * 86_400)
            );
        }

        /// Any input gives a value or an error that names the input. An `Ok` value is a duration.
        #[test]
        fn parse_age_never_panics_and_names_a_bad_input(raw in "\\PC{0,24}") {
            match parse_age(&raw) {
                Ok(_) => {}
                Err(e) => prop_assert!(e.to_string().contains(&format!("{raw:?}")), "{e}"),
            }
        }

        #[test]
        fn age_text_ends_in_a_unit_and_reads_back(secs in 0u64..10_000_000_000) {
            let text = format_age(Duration::from_secs(secs));
            prop_assert!(text.ends_with(['s', 'm', 'h', 'd']), "{text}");
            let words: Vec<String> = text.split(' ').map(str::to_owned).collect();
            prop_assert!(words.iter().all(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit())));
            prop_assert_eq!(parse_age(&text.replace(' ', "")).expect("reads back"), Duration::from_secs(secs));
        }
    }
}
