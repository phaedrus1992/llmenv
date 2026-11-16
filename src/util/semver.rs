//! A version such as `2.1.295`, read as three numbers.

/// The largest number of one part that the readers accept. A larger part is not a real version of
/// a tool, and the cap keeps a garbled text from passing as one.
const MAX_PART: u64 = 999_999_999;

/// Parse `X.Y.Z` with an optional leading `v` and an optional `-pre-release` or `+build` suffix.
/// `None` when the text is not a semantic version, or a part is above [`MAX_PART`].
fn parse(text: &str) -> Option<semver::Version> {
    // The crate rejects a leading `v`, which the tools that llmenv reads sometimes print.
    let version = semver::Version::parse(text.strip_prefix('v').unwrap_or(text)).ok()?;
    [version.major, version.minor, version.patch]
        .iter()
        .all(|part| *part <= MAX_PART)
        .then_some(version)
}

/// The three numbers of a version, or `None` when the text is not a version.
pub(crate) fn parse_semver_triple(text: &str) -> Option<(u64, u64, u64)> {
    parse(text).map(|v| (v.major, v.minor, v.patch))
}

/// Whether `version` is a release at or after `since`, or `None` when it is not a version. A
/// pre-release of `since` itself, such as `2.1.295-beta` for 2.1.295, sorts before the release,
/// so it is `false`.
pub(crate) fn is_release_at_least(version: &str, since: (u64, u64, u64)) -> Option<bool> {
    // Semver sorts a pre-release below the release of the same numbers.
    Some(parse(version)? >= semver::Version::new(since.0, since.1, since.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_prerelease_of_the_same_numbers_is_not_at_least_the_release() {
        let since = (2, 1, 295);
        assert_eq!(is_release_at_least("2.1.295-beta", since), Some(false));
        assert_eq!(
            is_release_at_least("2.1.295-rc.1+build", since),
            Some(false)
        );
        assert_eq!(is_release_at_least("2.1.295", since), Some(true));
        assert_eq!(is_release_at_least("2.1.295+build-5", since), Some(true));
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
        /// A part above the cap is not a version, in any of the three places.
        #[test]
        fn a_part_above_the_cap_is_not_a_version(
            big in 1_000_000_000u64..=u64::MAX,
            place in 0usize..3,
        ) {
            let mut parts = [1u64, 2, 3];
            parts[place] = big;
            let text = format!("{}.{}.{}", parts[0], parts[1], parts[2]);
            prop_assert_eq!(parse_semver_triple(&text), None);
        }

        /// A `+build` suffix does not change the answer of the release gate.
        #[test]
        fn a_build_suffix_does_not_change_the_release_gate(
            a in 0u64..4, b in 0u64..12, c in 0u64..400,
            pre in any::<bool>(),
        ) {
            let base = if pre { format!("{a}.{b}.{c}-beta") } else { format!("{a}.{b}.{c}") };
            let since = (2, 1, 304);
            prop_assert_eq!(
                is_release_at_least(&format!("{base}+build.7"), since),
                is_release_at_least(&base, since)
            );
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
