//! Value-shape merge for engine capabilities.
//!
//! Capabilities are contributed by multiple sources — the top-level config and
//! each selected bundle's `bundle.yaml`. They compose by **value shape**, not
//! key identity (see `docs/design/engine-capabilities.md`, D2):
//!
//! - **Lists** (`allow`/`ask`/`deny`, `hooks`, `plugins`, and the per-engine
//!   `native` rule lists) → concatenate across contributors, then dedup.
//!   Order-independent union; no winner problem.
//! - **Scalars** (`default_mode`) → the highest-precedence contributor wins.
//!   Two contributors at the **same** precedence setting different values is an
//!   unresolvable ambiguity → hard-error naming both. Loud beats silent.

use std::collections::BTreeMap;

use crate::config::{Capabilities, NativePermissionRules, PermissionMode, Permissions};
use crate::util::{dedup, merge_yaml, normalize_yaml};

/// A single source of capability fragments. `precedence` encodes scope rank
/// (higher wins for scalars); `name` is used only in collision errors.
#[derive(Debug, Clone)]
pub struct CapabilityContributor {
    pub name: String,
    pub precedence: u8,
    pub capabilities: Capabilities,
}

/// Merge capability fragments from all contributors by value shape.
///
/// Contributors may be supplied in any order — precedence is read from the
/// `precedence` field, not from position.
///
/// # Errors
/// Returns an error when two contributors at the same (highest) precedence set
/// a scalar (`default_mode`) to different values, since there is no rank to
/// break the tie.
pub fn merge_capabilities(contributors: &[CapabilityContributor]) -> anyhow::Result<Capabilities> {
    let mut hooks = Vec::new();
    let mut plugins = Vec::new();
    let mut allow = Vec::new();
    let mut ask = Vec::new();
    let mut deny = Vec::new();
    let mut native_permissions: BTreeMap<String, NativePermissionRules> = BTreeMap::new();

    for c in contributors {
        let caps = &c.capabilities;
        hooks.extend(caps.hooks.iter().cloned());
        plugins.extend(caps.plugins.iter().cloned());
        allow.extend(caps.permissions.allow.iter().cloned());
        ask.extend(caps.permissions.ask.iter().cloned());
        deny.extend(caps.permissions.deny.iter().cloned());
        for (engine, rules) in &caps.native_permissions {
            let slot = native_permissions.entry(engine.clone()).or_default();
            slot.allow.extend(rules.allow.iter().cloned());
            slot.ask.extend(rules.ask.iter().cloned());
            slot.deny.extend(rules.deny.iter().cloned());
        }
    }

    dedup(&mut hooks);
    dedup(&mut plugins);
    dedup(&mut allow);
    dedup(&mut ask);
    dedup(&mut deny);
    for rules in native_permissions.values_mut() {
        dedup(&mut rules.allow);
        dedup(&mut rules.ask);
        dedup(&mut rules.deny);
    }

    let default_mode = resolve_default_mode(contributors)?;
    let native_hooks = merge_native_feature(contributors, |c| &c.native_hooks);
    let native_plugins = merge_native_feature(contributors, |c| &c.native_plugins);
    let native_mcp = merge_native_feature(contributors, |c| &c.native_mcp);
    let native = merge_native_flat(contributors);

    // Scalar resolution: highest precedence wins (not positional order).
    // #227: resolve by explicit precedence comparison, matching resolve_default_mode.
    let auto_memory_enabled = contributors
        .iter()
        .filter_map(|c| {
            c.capabilities
                .auto_memory_enabled
                .map(|v| (c.precedence, v))
        })
        .max_by_key(|(p, _)| *p)
        .map(|(_, v)| v);

    Ok(Capabilities {
        permissions: Permissions {
            default_mode,
            allow,
            ask,
            deny,
        },
        hooks,
        plugins,
        auto_memory_enabled,
        native_permissions,
        native_hooks,
        native_plugins,
        native_mcp,
        native,
    })
}

/// Merge one of the per-engine opaque `native_*` maps across all contributors.
///
/// Each engine's fragment is deep-merged ([`merge_yaml`]) in ascending
/// precedence order so the highest-precedence contributor wins on any scalar
/// collision (sequences concat, mappings union — see `merge_yaml`).
fn merge_native_feature(
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

/// Merge the flat `native:` map across all contributors.
///
/// The flat `native:` map has the same structure as per-engine maps: each
/// top-level key is deep-merged ([`merge_yaml`]) across contributors in
/// ascending precedence order so the highest-precedence contributor wins on any
/// scalar collision.  Delegates to [`merge_native_feature`] with the `native`
/// field accessor.
fn merge_native_flat(
    contributors: &[CapabilityContributor],
) -> BTreeMap<String, serde_yaml::Value> {
    merge_native_feature(contributors, |c| &c.native)
}

/// Resolve the `default_mode` scalar across contributors: highest precedence
/// wins; same-precedence disagreement is a hard error.
fn resolve_default_mode(
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::{Hook, HookHandler, HookHandlerKind, PermissionRule};

    fn rule(tool: &str, pattern: &str) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            pattern: Some(pattern.into()),
            paths: Vec::new(),
        }
    }

    fn hook(event: &str, cmd: &str) -> Hook {
        Hook {
            event: event.into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some(cmd.into()),
                tool: None,
            },
            bundle_origin: None,
        }
    }

    fn contributor(name: &str, precedence: u8, caps: Capabilities) -> CapabilityContributor {
        CapabilityContributor {
            name: name.into(),
            precedence,
            capabilities: caps,
        }
    }

    fn with_allow(rules: Vec<PermissionRule>) -> Capabilities {
        Capabilities {
            permissions: Permissions {
                allow: rules,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn empty_contributors_yield_empty_capabilities() {
        let out = merge_capabilities(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn lists_concatenate_across_contributors() {
        let a = contributor("a", 0, with_allow(vec![rule("Bash", "git diff *")]));
        let b = contributor("b", 1, with_allow(vec![rule("Read", "./src")]));
        let out = merge_capabilities(&[a, b]).unwrap();
        assert_eq!(out.permissions.allow.len(), 2);
    }

    #[test]
    fn duplicate_list_entries_are_deduped() {
        let shared = rule("Bash", "cargo *");
        let a = contributor("a", 0, with_allow(vec![shared.clone()]));
        let b = contributor(
            "b",
            1,
            with_allow(vec![shared.clone(), rule("Read", "./src")]),
        );
        let out = merge_capabilities(&[a, b]).unwrap();
        assert_eq!(out.permissions.allow, vec![shared, rule("Read", "./src")]);
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        let a = contributor(
            "a",
            0,
            with_allow(vec![rule("A", "1"), rule("B", "2"), rule("A", "1")]),
        );
        let out = merge_capabilities(&[a]).unwrap();
        assert_eq!(out.permissions.allow, vec![rule("A", "1"), rule("B", "2")]);
    }

    #[test]
    fn hooks_and_plugins_concat_and_dedup() {
        let caps_a = Capabilities {
            hooks: vec![hook("PreToolUse", "h.sh")],
            plugins: vec!["m:p".into()],
            ..Default::default()
        };
        let caps_b = Capabilities {
            hooks: vec![hook("PreToolUse", "h.sh"), hook("Stop", "s.sh")],
            plugins: vec!["m:p".into(), "m:q".into()],
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 1, caps_b)])
            .unwrap();
        assert_eq!(out.hooks.len(), 2);
        assert_eq!(out.plugins, vec!["m:p".to_string(), "m:q".to_string()]);
    }

    #[test]
    fn native_feature_maps_merge_per_engine() {
        // native_hooks/native_plugins/native_mcp are per-engine opaque YAML
        // fragments. Across contributors, the same engine's mapping deep-merges
        // (keys union, sequences concat).
        fn caps_with_native_hooks(engine: &str, yaml: &str) -> Capabilities {
            let mut m = BTreeMap::new();
            m.insert(engine.to_string(), serde_yaml::from_str(yaml).unwrap());
            Capabilities {
                native_hooks: m,
                ..Default::default()
            }
        }
        let a = caps_with_native_hooks("claude_code", "seq: [one]\nscalar_a: 1");
        let b = caps_with_native_hooks("claude_code", "seq: [two]\nscalar_b: 2");
        let out = merge_capabilities(&[contributor("a", 0, a), contributor("b", 1, b)]).unwrap();
        let merged = &out.native_hooks["claude_code"];
        let map = merged.as_mapping().expect("mapping");
        // sequences under the same key concatenate
        let seq = map
            .get(serde_yaml::Value::String("seq".into()))
            .and_then(|v| v.as_sequence())
            .expect("seq");
        assert_eq!(seq.len(), 2, "sequences concat: {seq:?}");
        // disjoint scalar keys both survive
        assert!(map.contains_key(serde_yaml::Value::String("scalar_a".into())));
        assert!(map.contains_key(serde_yaml::Value::String("scalar_b".into())));
    }

    #[test]
    fn native_rule_lists_merge_per_engine() {
        let mut native_a = BTreeMap::new();
        native_a.insert(
            "claude_code".to_string(),
            NativePermissionRules {
                deny: vec!["WebFetch(domain:a)".into()],
                ..Default::default()
            },
        );
        let mut native_b = BTreeMap::new();
        native_b.insert(
            "claude_code".to_string(),
            NativePermissionRules {
                deny: vec!["WebFetch(domain:b)".into()],
                ..Default::default()
            },
        );
        let caps_a = Capabilities {
            native_permissions: native_a,
            ..Default::default()
        };
        let caps_b = Capabilities {
            native_permissions: native_b,
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 0, caps_b)])
            .unwrap();
        assert_eq!(out.native_permissions["claude_code"].deny.len(), 2);
    }

    fn with_mode(mode: PermissionMode) -> Capabilities {
        Capabilities {
            permissions: Permissions {
                default_mode: Some(mode),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn higher_precedence_scalar_wins() {
        let low = contributor("low", 0, with_mode(PermissionMode::Default));
        let high = contributor("high", 2, with_mode(PermissionMode::AcceptEdits));
        let out = merge_capabilities(&[low, high]).unwrap();
        assert_eq!(
            out.permissions.default_mode,
            Some(PermissionMode::AcceptEdits)
        );
    }

    #[test]
    fn higher_precedence_wins_regardless_of_input_order() {
        let high = contributor("high", 2, with_mode(PermissionMode::AcceptEdits));
        let low = contributor("low", 0, with_mode(PermissionMode::Default));
        let out = merge_capabilities(&[high, low]).unwrap();
        assert_eq!(
            out.permissions.default_mode,
            Some(PermissionMode::AcceptEdits)
        );
    }

    #[test]
    fn same_precedence_same_value_is_not_a_conflict() {
        let a = contributor("a", 1, with_mode(PermissionMode::Plan));
        let b = contributor("b", 1, with_mode(PermissionMode::Plan));
        let out = merge_capabilities(&[a, b]).unwrap();
        assert_eq!(out.permissions.default_mode, Some(PermissionMode::Plan));
    }

    #[test]
    fn same_precedence_different_value_hard_errors() {
        let a = contributor("a", 1, with_mode(PermissionMode::Plan));
        let b = contributor("b", 1, with_mode(PermissionMode::AcceptEdits));
        let err = merge_capabilities(&[a, b]).unwrap_err().to_string();
        assert!(err.contains("conflicting default_mode"), "got: {err}");
        assert!(err.contains('a') && err.contains('b'), "got: {err}");
    }

    #[test]
    fn no_contributor_sets_mode_leaves_it_none() {
        let a = contributor("a", 0, with_allow(vec![rule("Bash", "x")]));
        let out = merge_capabilities(&[a]).unwrap();
        assert_eq!(out.permissions.default_mode, None);
    }

    // #291: native: blocks from bundle.yaml must be merged into the output.
    #[test]
    fn bundle_native_is_merged_into_capabilities() {
        let mut native = BTreeMap::new();
        native.insert(
            "claude_code".to_string(),
            serde_yaml::from_str::<serde_yaml::Value>("statusLine: test").unwrap(),
        );
        let caps = Capabilities {
            native,
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("bundle 'x'", 1, caps)]).unwrap();
        assert!(
            out.native.contains_key("claude_code"),
            "native: from bundle must appear in merged output"
        );
    }

    #[test]
    fn native_blocks_deep_merge_across_contributors() {
        let mut native_a = BTreeMap::new();
        native_a.insert(
            "claude_code".to_string(),
            serde_yaml::from_str::<serde_yaml::Value>("keyA: 1").unwrap(),
        );
        let mut native_b = BTreeMap::new();
        native_b.insert(
            "claude_code".to_string(),
            serde_yaml::from_str::<serde_yaml::Value>("keyB: 2").unwrap(),
        );
        let caps_a = Capabilities {
            native: native_a,
            ..Default::default()
        };
        let caps_b = Capabilities {
            native: native_b,
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 1, caps_b)])
            .unwrap();
        let map = out.native["claude_code"].as_mapping().expect("mapping");
        assert!(
            map.contains_key(serde_yaml::Value::String("keyA".into())),
            "keyA from lower-precedence contributor must survive"
        );
        assert!(
            map.contains_key(serde_yaml::Value::String("keyB".into())),
            "keyB from higher-precedence contributor must survive"
        );
    }

    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::collections::BTreeSet;

        fn arb_rule() -> impl Strategy<Value = PermissionRule> {
            ("[A-Za-z]{1,6}", "[a-z*]{1,6}").prop_map(|(tool, pat)| PermissionRule {
                tool,
                pattern: Some(pat),
                paths: Vec::new(),
            })
        }

        // Contributors carrying only list fields (allow rules + plugins) plus a
        // small native: map, so the scalar default_mode never forces a
        // same-precedence conflict.
        //
        // Varies engine names, precedence (1-10), and number of engines (0-3) to
        // exercise merge_native_flat across realistic multi-engine,
        // multi-precedence scenarios.
        fn arb_list_contributor() -> impl Strategy<Value = CapabilityContributor> {
            let arb_engine_entry =
                ("[a-z_]{1,8}", "[a-z]{1,4}", "[a-z]{1,6}").prop_map(|(engine, key, val)| {
                    let mut m = serde_yaml::Mapping::new();
                    m.insert(
                        serde_yaml::Value::String(key),
                        serde_yaml::Value::String(val),
                    );
                    (engine, serde_yaml::Value::Mapping(m))
                });
            (
                "[a-z]{1,4}",
                1u8..=10,
                prop::collection::vec(arb_rule(), 0..4),
                prop::collection::vec("[a-z:]{1,6}", 0..4),
                prop::collection::vec(arb_engine_entry, 0..3),
            )
                .prop_map(|(name, precedence, allow, plugins, engine_entries)| {
                    let native = engine_entries.into_iter().collect::<BTreeMap<_, _>>();
                    CapabilityContributor {
                        name,
                        precedence,
                        capabilities: Capabilities {
                            permissions: Permissions {
                                allow,
                                ..Default::default()
                            },
                            plugins,
                            native,
                            ..Default::default()
                        },
                    }
                })
        }

        fn allow_set(caps: &Capabilities) -> BTreeSet<PermissionRule> {
            caps.permissions.allow.iter().cloned().collect()
        }

        fn plugin_set(caps: &Capabilities) -> BTreeSet<String> {
            caps.plugins.iter().cloned().collect()
        }

        proptest! {
            // Merging is idempotent: feeding the merged output back through
            // merge_capabilities as a single contributor changes nothing.
            #[test]
            fn merge_is_idempotent(
                contribs in prop::collection::vec(arb_list_contributor(), 0..5)
            ) {
                let once = merge_capabilities(&contribs).unwrap();
                let again = merge_capabilities(&[CapabilityContributor {
                    name: "merged".into(),
                    precedence: 1,
                    capabilities: once.clone(),
                }])
                .unwrap();
                prop_assert_eq!(once, again);
            }

            // native_* fragments are normalized on first insert, so a single
            // contributor whose fragment carries duplicate sequence elements
            // produces the same output as re-merging that output. This guards the
            // insert-path normalization (the merge path was always normalized).
            #[test]
            fn native_fragment_insert_is_idempotent(
                items in prop::collection::vec("[a-z]{1,4}", 0..6),
            ) {
                let seq = serde_yaml::Value::Sequence(
                    items
                        .iter()
                        .map(|s| serde_yaml::Value::String(s.clone()))
                        .collect(),
                );
                let mut frag = serde_yaml::Mapping::new();
                frag.insert(serde_yaml::Value::String("list".into()), seq);
                let fragment = serde_yaml::Value::Mapping(frag);

                let mut native_hooks = BTreeMap::new();
                native_hooks.insert("claude_code".to_owned(), fragment);

                let contrib = CapabilityContributor {
                    name: "only".into(),
                    precedence: 1,
                    capabilities: Capabilities {
                        native_hooks,
                        ..Default::default()
                    },
                };

                let once = merge_capabilities(&[contrib]).unwrap();
                let again = merge_capabilities(&[CapabilityContributor {
                    name: "merged".into(),
                    precedence: 1,
                    capabilities: once.clone(),
                }])
                .unwrap();
                prop_assert_eq!(once, again);
            }

            // List union is order-independent: permuting contributors yields the
            // same *set* of allow rules and plugins (first-seen order may differ,
            // but membership is invariant).
            #[test]
            fn list_union_is_order_independent(
                contribs in prop::collection::vec(arb_list_contributor(), 0..5)
            ) {
                let forward = merge_capabilities(&contribs).unwrap();
                let mut reversed = contribs.clone();
                reversed.reverse();
                let backward = merge_capabilities(&reversed).unwrap();
                prop_assert_eq!(allow_set(&forward), allow_set(&backward));
                prop_assert_eq!(plugin_set(&forward), plugin_set(&backward));
            }

            // Output lists carry no duplicates.
            #[test]
            fn merged_lists_have_no_duplicates(
                contribs in prop::collection::vec(arb_list_contributor(), 0..5)
            ) {
                let out = merge_capabilities(&contribs).unwrap();
                let allow_len = out.permissions.allow.len();
                prop_assert_eq!(allow_len, allow_set(&out).len());
                let plugin_len = out.plugins.len();
                prop_assert_eq!(plugin_len, plugin_set(&out).len());
            }

            // The strictly-highest-precedence contributor's default_mode always
            // wins, regardless of input order or how many lower contributors set
            // a (possibly different) mode.
            #[test]
            fn highest_precedence_mode_wins(
                lows in prop::collection::vec(0u8..50, 0..5),
                winner_bump in 1u8..50,
            ) {
                let winner_prec = 50u8 + winner_bump;
                let mut contribs: Vec<CapabilityContributor> = lows
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| CapabilityContributor {
                        name: format!("low{i}"),
                        precedence: p,
                        capabilities: Capabilities {
                            permissions: Permissions {
                                default_mode: Some(PermissionMode::Default),
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    })
                    .collect();
                contribs.push(CapabilityContributor {
                    name: "winner".into(),
                    precedence: winner_prec,
                    capabilities: Capabilities {
                        permissions: Permissions {
                            default_mode: Some(PermissionMode::BypassPermissions),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                });
                let out = merge_capabilities(&contribs).unwrap();
                prop_assert_eq!(
                    out.permissions.default_mode,
                    Some(PermissionMode::BypassPermissions)
                );
            }
        }
    }
}
