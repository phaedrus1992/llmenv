//! The one age check for a stored time that expires.

/// Whether `ts` is no later than `now` and less than `limit` before it. `ts`, `now`, and `limit`
/// use one unit, such as seconds. A time in the future reads as old, so a clock that stepped back
/// cannot keep an entry alive (#3889).
pub fn is_within(ts: i64, now: i64, limit: i64) -> bool {
    now.checked_sub(ts)
        .is_some_and(|age| (0..limit).contains(&age))
}

#[cfg(test)]
mod tests {
    use super::is_within;
    use proptest::prelude::*;

    #[test]
    fn the_edges_of_the_window() {
        assert!(is_within(100, 100, 10), "the same instant is inside");
        assert!(is_within(91, 100, 10), "one before the limit is inside");
        assert!(!is_within(90, 100, 10), "the limit itself is outside");
        assert!(!is_within(101, 100, 10), "a future time is outside");
        assert!(
            !is_within(i64::MIN, 100, 10),
            "an overflowing age is outside"
        );
    }

    proptest! {
        /// A time is inside the window exactly when its age is in `0..limit`.
        #[test]
        fn inside_means_an_age_in_the_window(
            ts in -1_000_000i64..1_000_000,
            now in -1_000_000i64..1_000_000,
            limit in -10i64..2_000_000,
        ) {
            prop_assert_eq!(is_within(ts, now, limit), (0..limit).contains(&(now - ts)));
        }
    }
}
