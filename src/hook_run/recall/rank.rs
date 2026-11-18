//! Rank of each recall tag by how specific its scope is. Lower rank runs first.

use std::collections::BTreeMap;

/// The rank of a tag no scope contributed, such as the OS tag.
pub(in crate::hook_run) const UNSCOPED_RANK: u8 = 6;

/// The rank of every tag from a project scope or `$LLMENV_EXTRA_TAGS`, and of every bundle.
pub(in crate::hook_run) const MOST_SPECIFIC_RANK: u8 = 1;

/// Rank each active tag by how specific its source scope is. A lower rank is more specific
/// and is recalled first. A tag from several scopes takes the lowest rank among them.
pub(in crate::hook_run) fn tag_specificity(
    active: &crate::scope::ActiveScopes,
) -> BTreeMap<String, u8> {
    let mut ranks: BTreeMap<String, u8> = active
        .tags
        .iter()
        .map(|tag| (tag.clone(), UNSCOPED_RANK))
        .collect();
    let mut lower = |tag: &String, rank: u8| {
        ranks
            .entry(tag.clone())
            .and_modify(|current| *current = (*current).min(rank))
            .or_insert(rank);
    };
    for scope in &active.scopes {
        let rank = match scope.kind {
            "project" => 1,
            "content" => 2,
            "network" => 3,
            "user" => 4,
            "host" => 5,
            _ => UNSCOPED_RANK,
        };
        scope.tags.iter().for_each(|tag| lower(tag, rank));
    }
    // `extra_tags` stand in for a project's `.llmenv.yaml`, so they rank as project tags.
    active
        .extra_tags
        .iter()
        .for_each(|tag| lower(tag, MOST_SPECIFIC_RANK));
    ranks
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::{UNSCOPED_RANK, tag_specificity};
    use crate::scope::{ActiveScope, ActiveScopes};

    /// The kind table, written out again so a broken table in `tag_specificity` cannot agree
    /// with itself (#2822).
    fn kind_rank(kind: &str) -> u8 {
        match kind {
            "project" => 1,
            "content" => 2,
            "network" => 3,
            "user" => 4,
            "host" => 5,
            _ => 6,
        }
    }

    const KINDS: [&str; 6] = ["project", "content", "network", "user", "host", "other"];
    const TAGS: [&str; 4] = ["a", "b", "c", "d"];

    fn tag_set() -> impl Strategy<Value = BTreeSet<String>> {
        proptest::sample::subsequence(TAGS.to_vec(), 0..=TAGS.len())
            .prop_map(|tags| tags.into_iter().map(String::from).collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Each tag takes the lowest rank among its sources: the kind of each scope that holds
        /// it, `MOST_SPECIFIC_RANK` for an extra tag, and `UNSCOPED_RANK` for a tag that no
        /// scope holds. So a tag from a more specific scope never ranks below the same tag from
        /// a less specific one.
        #[test]
        fn each_tag_takes_its_most_specific_source(
            scopes in proptest::collection::vec(
                (proptest::sample::select(KINDS.to_vec()), tag_set()),
                0..6,
            ),
            extra in tag_set(),
            loose in tag_set(),
        ) {
            let scopes: Vec<ActiveScope> = scopes
                .into_iter()
                .map(|(kind, tags)| ActiveScope {
                    kind,
                    tags: tags.into_iter().collect(),
                    ..ActiveScope::default()
                })
                .collect();
            let mut tags: BTreeSet<String> =
                scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
            tags.extend(extra.iter().cloned());
            tags.extend(loose.iter().cloned());
            let active = ActiveScopes {
                unknown_content: Default::default(),
                scopes,
                tags: tags.clone(),
                extra_tags: extra.clone(),
            };

            let ranks = tag_specificity(&active);

            let mut expected: BTreeMap<String, u8> =
                tags.iter().map(|t| (t.clone(), UNSCOPED_RANK)).collect();
            for scope in &active.scopes {
                for tag in &scope.tags {
                    let slot = expected.entry(tag.clone()).or_insert(UNSCOPED_RANK);
                    *slot = (*slot).min(kind_rank(scope.kind));
                }
            }
            // An extra tag ranks as a project tag. 1 is the lowest rank, so it wins outright.
            for tag in &extra {
                expected.insert(tag.clone(), kind_rank("project"));
            }
            prop_assert_eq!(ranks, expected);
        }
    }
}
