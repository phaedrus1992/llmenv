//! A version such as `2.1.295`, read as three numbers.

/// Parse `X.Y.Z` with an optional leading `v` and an optional `-suffix` or
/// `+suffix` on Z. `None` when the text does not have that shape.
///
/// A hand-written parse keeps a whole crate out of the tree for one comparison.
pub(crate) fn parse_semver_triple(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.strip_prefix('v').unwrap_or(text);
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut next = || {
        let part = parts.next()?;
        // `[0-9]{1,9}`: `char::is_numeric` would accept full-width digits.
        let is_digits = (1..=9).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit());
        if is_digits {
            part.parse::<u64>().ok()
        } else {
            None
        }
    };
    let triple = (next()?, next()?, next()?);
    parts.next().is_none().then_some(triple)
}

/// Whether `version` is a release at or after `since`, or `None` when it is not a version. A
/// pre-release of `since` itself, such as `2.1.295-beta` for 2.1.295, sorts before the release,
/// so it is `false`.
pub(crate) fn is_release_at_least(version: &str, since: (u64, u64, u64)) -> Option<bool> {
    let triple = parse_semver_triple(version)?;
    Some(triple > since || (triple == since && !is_prerelease(version)))
}

/// Whether `text` is a pre-release, such as `2.1.295-beta`. Semver sorts a pre-release before
/// the release of the same three numbers. A `+build` suffix is not a pre-release.
pub(crate) fn is_prerelease(text: &str) -> bool {
    text.split('+')
        .next()
        .is_some_and(|core| core.contains('-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_prerelease_has_a_dash_before_any_build_suffix() {
        assert!(is_prerelease("2.1.295-beta"));
        assert!(is_prerelease("2.1.295-rc.1+build"));
        assert!(!is_prerelease("2.1.295"));
        assert!(!is_prerelease("2.1.295+build-5"));
    }

    #[test]
    fn parse_semver_triple_accepts_the_expected_shapes() {
        assert_eq!(parse_semver_triple("0.11.0"), Some((0, 11, 0)));
        assert_eq!(parse_semver_triple("v0.10.8"), Some((0, 10, 8)));
        assert_eq!(parse_semver_triple("0.11.0-rc.1"), Some((0, 11, 0)));
        assert_eq!(parse_semver_triple("1.2.3+build"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_semver_triple_rejects_other_shapes() {
        for bad in [
            "0.11",
            "0.11.0.1",
            "0.x.0",
            "\u{ff10}.11.0",
            "",
            "1234567890.0.0",
        ] {
            assert_eq!(parse_semver_triple(bad), None, "{bad:?}");
        }
    }

    proptest! {
        /// Any triple that fits nine digits round-trips through the text form, with or without
        /// a `v` prefix and a suffix.
        #[test]
        fn parse_semver_triple_round_trips(
            a in 0u64..1_000_000_000,
            b in 0u64..1_000_000_000,
            c in 0u64..1_000_000_000,
            prefix in prop::sample::select(vec!["", "v"]),
            suffix in prop::sample::select(vec!["", "-rc.1", "+build.5"]),
        ) {
            let text = format!("{prefix}{a}.{b}.{c}{suffix}");
            prop_assert_eq!(parse_semver_triple(&text), Some((a, b, c)));
        }

        /// The parser never panics, and only accepts text that starts with a digit or `v`.
        #[test]
        fn parse_semver_triple_never_panics(text in "\\PC{0,24}") {
            if parse_semver_triple(&text).is_some() {
                prop_assert!(text.starts_with(|c: char| c.is_ascii_digit() || c == 'v'));
            }
        }
    }

    proptest! {
        /// A release is at least `since` exactly when its numbers are, and a pre-release of
        /// `since` itself is not.
        #[test]
        fn a_release_gate_orders_by_numbers_and_puts_a_prerelease_first(
            a in 0u64..4, b in 0u64..12, c in 0u64..400,
            pre in any::<bool>(),
        ) {
            let since = (2, 1, 304);
            let text = if pre { format!("{a}.{b}.{c}-beta") } else { format!("{a}.{b}.{c}") };
            let after = a > 2 || (a == 2 && (b > 1 || (b == 1 && c > 304)));
            let at = (a, b, c) == since;
            prop_assert_eq!(is_release_at_least(&text, since), Some(after || (at && !pre)));
        }
    }
}
