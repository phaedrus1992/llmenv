//! Precedence-based resolution of merged capability fields.
//!
//! The scalar, map, and native-fragment merges that `merge_capabilities` calls. Each
//! helper picks its winner by contributor precedence (see `capabilities.rs`).

use std::collections::BTreeMap;

use crate::config::Capabilities;
use crate::merge::capabilities::CapabilityContributor;
use crate::util::{merge_yaml, normalize_yaml};

/// Merge one of the per-engine opaque `native_*` maps across all contributors.
///
/// Each engine's fragment is deep-merged ([`merge_yaml`]) in ascending
/// precedence order so the highest-precedence contributor wins on any scalar
/// collision (sequences concat, mappings union — see `merge_yaml`).
pub(super) fn merge_native_feature(
    contributors: &[CapabilityContributor],
    select: impl Fn(&Capabilities) -> &BTreeMap<String, serde_yaml::Value>,
) -> BTreeMap<String, serde_yaml::Value> {
    let mut ordered: Vec<&CapabilityContributor> = contributors.iter().collect();
    ordered.sort_by_key(|c| c.precedence);

    let mut merged: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
    for c in ordered {
        for (engine, fragment) in select(&c.capabilities) {
            match merged.get_mut(engine) {
                Some(existing) => merge_yaml(existing, fragment.clone()),
                None => {
                    // Normalize on first insert so the result matches what the
                    // merge path produces — otherwise a fragment with duplicate
                    // sequence elements keeps them on insert but loses them on a
                    // later merge, making this function non-idempotent.
                    let mut fragment = fragment.clone();
                    normalize_yaml(&mut fragment);
                    merged.insert(engine.clone(), fragment);
                }
            }
        }
    }
    merged
}

/// Fail when two entries at the same precedence hold different values.
///
/// Every pair is checked, at every precedence tier. A tie below the winning tier is still a
/// conflict, so the result does not depend on the order of the contributors (#3329 review).
fn check_same_precedence_ties<V: PartialEq + std::fmt::Debug>(
    what: &str,
    entries: &[(&CapabilityContributor, &V)],
) -> anyhow::Result<()> {
    let mut first_at: BTreeMap<u8, (&CapabilityContributor, &V)> = BTreeMap::new();
    for &(c, value) in entries {
        match first_at.get(&c.precedence).copied() {
            None => {
                first_at.insert(c.precedence, (c, value));
            }
            Some((first, first_value)) if first_value != value => {
                anyhow::bail!(
                    "conflicting {what} at the same precedence: \
                     '{}' sets {first_value:?} but '{}' sets {value:?} — no scope can break \
                     the tie; resolve by giving one a higher-precedence scope",
                    first.name,
                    c.name,
                );
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Resolve a scalar `Capabilities`/`features.*` field by highest-precedence-wins:
/// the value from the contributor with the highest `precedence` that set it
/// (#1023/#1025 — shared by the scalar fields that all followed this same
/// filter_map/max_by_key pattern, at both the `Capabilities` and `Features` level).
///
/// Same-precedence disagreement is a hard error naming both contributors, for every
/// scalar (#1215 — this helper used to pick the last-seen value silently instead, an
/// undocumented, iteration-order-dependent divergence from that promise).
pub(super) fn highest_precedence<T: Clone + PartialEq + std::fmt::Debug>(
    contributors: &[CapabilityContributor],
    field_name: &str,
    pick: impl Fn(&Capabilities) -> Option<&T>,
) -> anyhow::Result<Option<T>> {
    let entries: Vec<(&CapabilityContributor, &T)> = contributors
        .iter()
        .filter_map(|c| pick(&c.capabilities).map(|v| (c, v)))
        .collect();
    check_same_precedence_ties(field_name, &entries)?;
    Ok(entries
        .iter()
        .max_by_key(|(c, _)| c.precedence)
        .map(|(_, v)| (*v).clone()))
}

/// Merge one map field across contributors: per key, the highest-precedence
/// contributor wins. Same-precedence disagreement on one key is a hard error
/// that names both contributors, the same policy as the scalar fields.
pub(super) fn resolve_per_key<V: Clone + PartialEq + std::fmt::Debug>(
    contributors: &[CapabilityContributor],
    label: &str,
    pick: impl Fn(&Capabilities) -> &BTreeMap<String, V>,
) -> anyhow::Result<BTreeMap<String, V>> {
    let mut by_key: BTreeMap<&String, Vec<(&CapabilityContributor, &V)>> = BTreeMap::new();
    for c in contributors {
        for (key, value) in pick(&c.capabilities) {
            by_key.entry(key).or_default().push((c, value));
        }
    }
    let mut result = BTreeMap::new();
    for (key, entries) in by_key {
        check_same_precedence_ties(&format!("{label} '{key}'"), &entries)?;
        if let Some((_, value)) = entries.iter().max_by_key(|(c, _)| c.precedence) {
            result.insert(key.clone(), (*value).clone());
        }
    }
    Ok(result)
}
