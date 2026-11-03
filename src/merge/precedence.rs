//! Precedence-based resolution of merged capability fields.
//!
//! The scalar, map, and native-fragment merges that `merge_capabilities` calls. Each
//! helper picks its winner by contributor precedence (see `capabilities.rs`).

use std::collections::BTreeMap;

use crate::config::{Capabilities, PermissionMode};
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

/// Resolve a scalar `Capabilities`/`features.*` field by highest-precedence-wins:
/// the value from the contributor with the highest `precedence` that set it
/// (#1023/#1025 — shared by the scalar fields that all followed this same
/// filter_map/max_by_key pattern, at both the `Capabilities` and `Features` level).
///
/// Same-precedence disagreement is a hard error naming both contributors,
/// matching [`resolve_default_mode`]'s policy and the module doc comment's
/// promise for every scalar (#1215 — this helper used to pick the last-seen
/// value silently instead, an undocumented, iteration-order-dependent
/// divergence from that promise).
pub(super) fn highest_precedence<T: Clone + PartialEq + std::fmt::Debug>(
    contributors: &[CapabilityContributor],
    field_name: &str,
    pick: impl Fn(&Capabilities) -> Option<&T>,
) -> anyhow::Result<Option<T>> {
    let mut winner: Option<(&CapabilityContributor, T)> = None;
    for c in contributors {
        let Some(v) = pick(&c.capabilities) else {
            continue;
        };
        let v = v.clone();
        match winner {
            None => winner = Some((c, v)),
            Some((prev_c, prev_v)) => {
                if c.precedence > prev_c.precedence {
                    winner = Some((c, v));
                } else if c.precedence == prev_c.precedence && v != prev_v {
                    anyhow::bail!(
                        "conflicting {field_name} at the same precedence: \
                         '{}' sets {prev_v:?} but '{}' sets {v:?} — no scope can break \
                         the tie; resolve by giving one a higher-precedence scope",
                        prev_c.name,
                        c.name,
                    );
                } else {
                    winner = Some((prev_c, prev_v));
                }
            }
        }
    }
    Ok(winner.map(|(_, v)| v))
}

/// Merge the flat `native:` map across all contributors.
///
/// The flat `native:` map has the same structure as per-engine maps: each
/// top-level key is deep-merged ([`merge_yaml`]) across contributors in
/// ascending precedence order so the highest-precedence contributor wins on any
/// scalar collision.  Delegates to [`merge_native_feature`] with the `native`
/// field accessor.
pub(super) fn merge_native_flat(
    contributors: &[CapabilityContributor],
) -> BTreeMap<String, serde_yaml::Value> {
    merge_native_feature(contributors, |c| &c.native)
}

/// Merge one map field across contributors: per key, the highest-precedence
/// contributor wins. Same-precedence disagreement on one key is a hard error
/// that names both contributors, the same policy as the scalar fields.
pub(super) fn resolve_per_key<V: Clone + PartialEq + std::fmt::Debug>(
    contributors: &[CapabilityContributor],
    label: &str,
    pick: impl Fn(&Capabilities) -> &BTreeMap<String, V>,
) -> anyhow::Result<BTreeMap<String, V>> {
    let mut result: BTreeMap<String, (&CapabilityContributor, &V)> = BTreeMap::new();
    for c in contributors {
        for (key, value) in pick(&c.capabilities) {
            match result.get(key) {
                Some((prev_c, _)) if c.precedence < prev_c.precedence => {}
                Some((prev_c, prev_value)) if c.precedence == prev_c.precedence => {
                    if value != *prev_value {
                        anyhow::bail!(
                            "conflicting {label} '{key}' at the same precedence: \
                             '{}' sets {:?} but '{}' sets {:?} — no scope can break \
                             the tie; resolve by giving one a higher-precedence scope",
                            prev_c.name,
                            prev_value,
                            c.name,
                            value,
                        );
                    }
                }
                _ => {
                    result.insert(key.clone(), (c, value));
                }
            }
        }
    }
    Ok(result
        .into_iter()
        .map(|(k, (_, v))| (k, v.clone()))
        .collect())
}

/// Resolve the `default_mode` scalar across contributors: highest precedence
/// wins; same-precedence disagreement is a hard error.
pub(super) fn resolve_default_mode(
    contributors: &[CapabilityContributor],
) -> anyhow::Result<Option<PermissionMode>> {
    let mut winner: Option<(&CapabilityContributor, PermissionMode)> = None;
    for c in contributors {
        let Some(mode) = c.capabilities.permissions.default_mode else {
            continue;
        };
        match winner {
            None => winner = Some((c, mode)),
            Some((prev_c, prev_mode)) => {
                if c.precedence > prev_c.precedence {
                    winner = Some((c, mode));
                } else if c.precedence == prev_c.precedence && mode != prev_mode {
                    anyhow::bail!(
                        "conflicting default_mode at the same precedence: \
                         '{}' sets {:?} but '{}' sets {:?} — no scope can break \
                         the tie; resolve by giving one a higher-precedence scope",
                        prev_c.name,
                        prev_mode,
                        c.name,
                        mode,
                    );
                }
                // c.precedence < prev: prev keeps winning. c.precedence == prev
                // with equal value: agreement, no-op.
            }
        }
    }
    Ok(winner.map(|(_, mode)| mode))
}
