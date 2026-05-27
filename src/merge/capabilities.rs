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
    let mut native: BTreeMap<String, NativePermissionRules> = BTreeMap::new();

    for c in contributors {
        let caps = &c.capabilities;
        hooks.extend(caps.hooks.iter().cloned());
        plugins.extend(caps.plugins.iter().cloned());
        allow.extend(caps.permissions.allow.iter().cloned());
        ask.extend(caps.permissions.ask.iter().cloned());
        deny.extend(caps.permissions.deny.iter().cloned());
        for (engine, rules) in &caps.permissions.native {
            let slot = native.entry(engine.clone()).or_default();
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
    for rules in native.values_mut() {
        dedup(&mut rules.allow);
        dedup(&mut rules.ask);
        dedup(&mut rules.deny);
    }

    let default_mode = resolve_default_mode(contributors)?;

    Ok(Capabilities {
        permissions: Permissions {
            default_mode,
            allow,
            ask,
            deny,
            native,
        },
        hooks,
        plugins,
    })
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

/// Stable dedup preserving first-seen order. Lists are small (rules, hooks,
/// plugin ids), so the quadratic scan is fine and avoids requiring `Hash`/`Ord`
/// on every element type.
fn dedup<T: PartialEq>(items: &mut Vec<T>) {
    let mut i = 0;
    while i < items.len() {
        if items[..i].contains(&items[i]) {
            items.remove(i);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
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
            permissions: Permissions {
                native: native_a,
                ..Default::default()
            },
            ..Default::default()
        };
        let caps_b = Capabilities {
            permissions: Permissions {
                native: native_b,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 0, caps_b)])
            .unwrap();
        assert_eq!(out.permissions.native["claude_code"].deny.len(), 2);
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
}
