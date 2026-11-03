//! Property tests for same-precedence ties in the merged `env` map (#3336).

use super::*;
use proptest::prelude::*;

/// Contributors with a small precedence range and a small env key space, so ties occur often.
fn arb_tie_contributors() -> impl Strategy<Value = Vec<CapabilityContributor>> {
    prop::collection::vec(
        (1u8..=3, prop::collection::vec(("[ab]", "[xy]"), 0..3)),
        0..6,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(|(i, (precedence, entries))| CapabilityContributor {
                name: format!("c{i}"),
                precedence,
                capabilities: Capabilities {
                    env: entries.into_iter().collect(),
                    ..Default::default()
                },
            })
            .collect()
    })
}

/// Whether two contributors at one precedence set one env key to different values.
fn has_tie_conflict(contribs: &[CapabilityContributor]) -> bool {
    for (i, a) in contribs.iter().enumerate() {
        for b in &contribs[i + 1..] {
            if a.precedence != b.precedence {
                continue;
            }
            for (key, value) in &a.capabilities.env {
                if b.capabilities
                    .env
                    .get(key)
                    .is_some_and(|other| other != value)
                {
                    return true;
                }
            }
        }
    }
    false
}

proptest! {
    // The merge fails exactly when a same-precedence tie disagrees, and the input order does not
    // change that answer.
    #[test]
    fn a_tie_is_an_error_exactly_when_same_precedence_values_disagree(
        contribs in arb_tie_contributors(),
    ) {
        let expected = has_tie_conflict(&contribs);
        let mut reversed = contribs.clone();
        reversed.reverse();
        prop_assert_eq!(merge_capabilities(&contribs).is_err(), expected);
        prop_assert_eq!(merge_capabilities(&reversed).is_err(), expected);
    }
}
