//! Timestamps and durations: RFC 3339 text, age text, and the `--older-than` value. All of it
//! goes through `jiff`, so no other crate is needed for time text (#3790).

use jiff::{SignedDuration, Span, SpanRelativeTo, SpanRound, Timestamp, Unit, fmt::friendly};
use std::time::{Duration, SystemTime};

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
/// Errors if `text` is not RFC 3339, or is outside the range of `SystemTime`.
pub fn parse_rfc3339(text: &str) -> anyhow::Result<SystemTime> {
    let ts: Timestamp = text
        .parse()
        .map_err(|e| anyhow::anyhow!("{text:?} is not an RFC 3339 timestamp: {e}"))?;
    Ok(SystemTime::from(ts))
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
        Err(_) => printer.duration_to_string(&signed),
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
    let signed = span
        .to_duration(SpanRelativeTo::days_are_24_hours())
        .map_err(|_| {
            anyhow::anyhow!(
                "the duration {raw:?} uses months or years, which have no fixed length. \
                 Give it in weeks or days, such as 52w or 365d"
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
    use super::*;
    use proptest::prelude::*;

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
        let err = parse_age("1y").expect_err("1y").to_string();
        assert!(err.contains("52w or 365d"), "{err}");
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
