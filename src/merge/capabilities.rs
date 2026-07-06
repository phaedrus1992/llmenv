//! Value-shape merge for engine capabilities.
//!
//! Capabilities are contributed by multiple sources — the top-level config and
//! each selected bundle's `bundle.yaml`. They compose by **value shape**, not
//! key identity (see `docs/design/engine-capabilities.md`, D2):
//!
//! - **Lists** (`allow`/`ask`/`deny`, `hooks`, `plugins`, `mcp`, and the
//!   per-engine `native` rule lists) → concatenate across contributors, then
//!   dedup. Order-independent union; no winner problem.
//! - **Scalars** (`default_mode`) → the highest-precedence contributor wins.
//!   Two contributors at the **same** precedence setting different values is an
//!   unresolvable ambiguity → hard-error naming both. Loud beats silent.
//! - **Maps** (`env`) → per-key scalar merge: highest-precedence contributor
//!   wins per key. Same-precedence disagreement on a single key is a hard error,
//!   matching the `default_mode` scalar policy.

use std::collections::BTreeMap;

use crate::config::{
    Capabilities, Features, HostEntry, Memory, NativePermissionRules, PermissionMode, Permissions,
    Throttle,
};
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
/// Returns an error when two contributors at the same precedence set a scalar
/// field (`default_mode`, or a single `env` key) to different values, since
/// there is no rank to break the tie.
pub fn merge_capabilities(contributors: &[CapabilityContributor]) -> anyhow::Result<Capabilities> {
    let mut hooks = Vec::new();
    let mut plugins = Vec::new();
    let mut mcp = Vec::new();
    let mut lsp = Vec::new();
    let mut skills = Vec::new();
    let mut allow = Vec::new();
    let mut ask = Vec::new();
    let mut deny = Vec::new();
    let mut native_permissions: BTreeMap<String, NativePermissionRules> = BTreeMap::new();

    // Sort contributors by precedence to ensure higher precedence wins for lists.
    let mut ordered: Vec<&CapabilityContributor> = contributors.iter().collect();
    ordered.sort_by_key(|c| c.precedence);

    for c in &ordered {
        let caps = &c.capabilities;
        hooks.extend(caps.hooks.iter().cloned());
        plugins.extend(caps.plugins.iter().cloned());
        mcp.extend(caps.mcp.iter().cloned());
        lsp.extend(caps.lsp.iter().cloned());
        skills.extend(caps.skills.iter().cloned());
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

    let env = resolve_env(contributors)?;

    dedup(&mut hooks);
    dedup(&mut plugins);
    dedup(&mut mcp);
    dedup(&mut lsp);
    dedup(&mut skills);
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
    let host = resolve_host_map(contributors)?;

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

    // Collect memory and throttle entries from all contributors: concat + dedup
    // (same list model as hooks, plugins, mcp). Ambiguity at resolve-time, not merge-time.
    let mut memory: Vec<Memory> = Vec::new();
    let mut throttle: Vec<Throttle> = Vec::new();
    for c in &ordered {
        if let Some(features) = &c.capabilities.features {
            memory.extend(features.memory.iter().cloned());
            throttle.extend(features.throttle.iter().cloned());
        }
    }
    dedup(&mut memory);
    dedup(&mut throttle);
    let features = if memory.is_empty() && throttle.is_empty() {
        None
    } else {
        Some(Features {
            memory,
            throttle,
            context_mode: None,
        })
    };

    Ok(Capabilities {
        permissions: Permissions {
            default_mode,
            allow,
            ask,
            deny,
        },
        hooks,
        plugins,
        mcp,
        lsp,
        skills,
        env,
        auto_memory_enabled,
        effort_level: None,
        advisor_size: None,
        native_permissions,
        native_hooks,
        native_plugins,
        native_mcp,
        native,
        features,
        host,
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

/// Merge the `host` address table across contributors: per key, highest-precedence
/// contributor wins. Same-precedence disagreement on a single key is a hard error,
/// matching the `default_mode` and `env` scalar policy.
fn resolve_host_map(
    contributors: &[CapabilityContributor],
) -> anyhow::Result<BTreeMap<String, HostEntry>> {
    let mut result: BTreeMap<String, (&CapabilityContributor, &HostEntry)> = BTreeMap::new();
    for c in contributors {
        for (name, entry) in &c.capabilities.host {
            match result.get(name) {
                None => {
                    result.insert(name.clone(), (c, entry));
                }
                Some((prev_c, prev_entry)) => {
                    if c.precedence > prev_c.precedence {
                        result.insert(name.clone(), (c, entry));
                    } else if c.precedence == prev_c.precedence && entry != *prev_entry {
                        anyhow::bail!(
                            "conflicting host entry '{name}' at the same precedence: \
                             '{}' and '{}' — resolve by giving one a higher-precedence scope",
                            prev_c.name,
                            c.name,
                        );
                    }
                    // c.precedence < prev: prev keeps winning.
                    // c.precedence == prev, same value: agreement, no-op.
                }
            }
        }
    }
    Ok(result
        .into_iter()
        .map(|(k, (_, v))| (k, v.clone()))
        .collect())
}

/// Resolve the `env` map across contributors: per key, highest-precedence
/// contributor wins. Same-precedence disagreement on any key is a hard error,
/// matching the `default_mode` scalar policy.
fn resolve_env(contributors: &[CapabilityContributor]) -> anyhow::Result<BTreeMap<String, String>> {
    // Track the winning (contributor, value) per key. Order-independent: all
    // four precedence cases (higher wins, lower loses, same+agree, same+conflict)
    // are handled by comparing stored vs incoming precedence.
    let mut env: BTreeMap<String, (&CapabilityContributor, &str)> = BTreeMap::new();

    for c in contributors {
        for (key, value) in &c.capabilities.env {
            match env.get(key) {
                None => {
                    env.insert(key.clone(), (c, value.as_str()));
                }
                Some((prev_c, prev_value)) => {
                    if c.precedence > prev_c.precedence {
                        env.insert(key.clone(), (c, value.as_str()));
                    } else if c.precedence == prev_c.precedence && value.as_str() != *prev_value {
                        anyhow::bail!(
                            "conflicting env key '{key}' at the same precedence: \
                             '{}' sets {:?} but '{}' sets {:?} — no scope can break \
                             the tie; resolve by giving one a higher-precedence scope",
                            prev_c.name,
                            prev_value,
                            c.name,
                            value,
                        );
                    }
                    // c.precedence < prev: prev keeps winning.
                    // c.precedence == prev, same value: agreement, no-op.
                }
            }
        }
    }

    Ok(env
        .into_iter()
        .map(|(k, (_, v))| (k, v.to_string()))
        .collect())
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

    fn with_env(pairs: &[(&str, &str)]) -> Capabilities {
        Capabilities {
            env: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    // #355: env key merge — disjoint keys from two contributors both survive.
    #[test]
    fn env_disjoint_keys_merge() {
        let a = contributor("a", 1, with_env(&[("A_VAR", "a")]));
        let b = contributor("b", 2, with_env(&[("B_VAR", "b")]));
        let out = merge_capabilities(&[a, b]).unwrap();
        assert_eq!(out.env.get("A_VAR").map(String::as_str), Some("a"));
        assert_eq!(out.env.get("B_VAR").map(String::as_str), Some("b"));
    }

    // #355: env key merge — higher-precedence contributor wins on key collision.
    #[test]
    fn env_higher_precedence_wins() {
        let low = contributor("low", 1, with_env(&[("KEY", "low_val")]));
        let high = contributor("high", 5, with_env(&[("KEY", "high_val")]));
        let out = merge_capabilities(&[low, high]).unwrap();
        assert_eq!(out.env.get("KEY").map(String::as_str), Some("high_val"));
    }

    // #355: higher-precedence wins regardless of input order.
    #[test]
    fn env_higher_precedence_wins_reversed_order() {
        let high = contributor("high", 5, with_env(&[("KEY", "high_val")]));
        let low = contributor("low", 1, with_env(&[("KEY", "low_val")]));
        let out = merge_capabilities(&[high, low]).unwrap();
        assert_eq!(out.env.get("KEY").map(String::as_str), Some("high_val"));
    }

    // #355: same-precedence, same-value agreement must not error.
    #[test]
    fn env_same_precedence_same_value_is_ok() {
        let a = contributor("a", 3, with_env(&[("KEY", "shared")]));
        let b = contributor("b", 3, with_env(&[("KEY", "shared")]));
        let out = merge_capabilities(&[a, b]).unwrap();
        assert_eq!(out.env.get("KEY").map(String::as_str), Some("shared"));
    }

    // #355: same-precedence, different-value conflict is a hard error.
    #[test]
    fn env_same_precedence_different_value_errors() {
        let a = contributor("bundle-a", 3, with_env(&[("MY_VAR", "alpha")]));
        let b = contributor("bundle-b", 3, with_env(&[("MY_VAR", "beta")]));
        let err = merge_capabilities(&[a, b]).unwrap_err().to_string();
        assert!(err.contains("conflicting env key"), "got: {err}");
        assert!(err.contains("MY_VAR"), "got: {err}");
        assert!(
            err.contains("bundle-a") && err.contains("bundle-b"),
            "got: {err}"
        );
    }

    // #355: conflict is detected even when higher-prec contributor is first in input.
    #[test]
    fn env_conflict_detected_regardless_of_order() {
        let b = contributor("b", 3, with_env(&[("VAR", "b_val")]));
        let a = contributor("a", 3, with_env(&[("VAR", "a_val")]));
        let err = merge_capabilities(&[b, a]).unwrap_err().to_string();
        assert!(err.contains("conflicting env key"), "got: {err}");
    }

    // #355: non-conflicting keys from same-precedence contributors both survive.
    #[test]
    fn env_same_precedence_disjoint_keys_ok() {
        let a = contributor("a", 3, with_env(&[("A", "1")]));
        let b = contributor("b", 3, with_env(&[("B", "2")]));
        let out = merge_capabilities(&[a, b]).unwrap();
        assert_eq!(out.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(out.env.get("B").map(String::as_str), Some("2"));
    }

    // #355: only the conflicting key errors; other keys do not matter.
    #[test]
    fn env_conflict_message_names_the_key() {
        let a = contributor("src-a", 2, with_env(&[("CONFLICT", "val1"), ("SAFE", "x")]));
        let b = contributor("src-b", 2, with_env(&[("CONFLICT", "val2")]));
        let err = merge_capabilities(&[a, b]).unwrap_err().to_string();
        assert!(err.contains("CONFLICT"), "got: {err}");
    }

    // #503: lsp entries from bundle.yaml must concatenate and dedup.
    #[test]
    fn lsp_entries_concatenate_across_contributors() {
        use crate::config::LspServer;
        let server = |name: &str| LspServer {
            name: name.into(),
            command: "cmd".into(),
            ..Default::default()
        };
        let caps_a = Capabilities {
            lsp: vec![server("rust-analyzer")],
            ..Default::default()
        };
        let caps_b = Capabilities {
            lsp: vec![server("clangd")],
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 1, caps_b)])
            .unwrap();
        assert_eq!(out.lsp.len(), 2, "both lsp entries must survive");
    }

    #[test]
    fn lsp_entries_are_deduped() {
        use crate::config::LspServer;
        let server = LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            filetypes: vec!["rust".into()],
            ..Default::default()
        };
        let caps_a = Capabilities {
            lsp: vec![server.clone()],
            ..Default::default()
        };
        let caps_b = Capabilities {
            lsp: vec![server],
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 1, caps_b)])
            .unwrap();
        assert_eq!(
            out.lsp.len(),
            1,
            "identical lsp entries must be deduped to one"
        );
    }

    // #503: tag selection — lsp entries with non-matching `when` must be excluded.
    // This exercises that `when` is preserved through merge so the adapter can
    // filter by active tags; the merge itself does NOT filter (it is tag-agnostic).
    #[test]
    fn lsp_when_tags_preserved_through_merge() {
        use crate::config::LspServer;
        let tagged = LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            when: vec!["rust".into()],
            ..Default::default()
        };
        let untagged = LspServer {
            name: "clangd".into(),
            command: "clangd".into(),
            when: vec![],
            ..Default::default()
        };
        let caps = Capabilities {
            lsp: vec![tagged, untagged],
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps)]).unwrap();
        // Both survive merge; callers filter by active_tags.
        assert_eq!(out.lsp.len(), 2, "both lsp entries must survive merge");
        // Verify when tags are intact.
        let ra = out.lsp.iter().find(|s| s.name == "rust-analyzer").unwrap();
        assert_eq!(ra.when, vec!["rust".to_string()]);
        let cl = out.lsp.iter().find(|s| s.name == "clangd").unwrap();
        assert!(cl.when.is_empty());
    }

    // #329: mcp entries from bundle.yaml must concatenate and dedup.
    #[test]
    fn mcp_entries_concatenate_across_contributors() {
        use crate::config::{McpServer, McpTransport};
        let server = |name: &str| McpServer {
            name: name.into(),
            when: vec![],
            transport: McpTransport::Stdio,
            command: Some("cmd".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            ..Default::default()
        };
        let caps_a = Capabilities {
            mcp: vec![server("ctx")],
            ..Default::default()
        };
        let caps_b = Capabilities {
            mcp: vec![server("playwright")],
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 1, caps_b)])
            .unwrap();
        assert_eq!(out.mcp.len(), 2);
    }

    #[test]
    fn mcp_entries_are_deduped() {
        use crate::config::{McpServer, McpTransport};
        let server = McpServer {
            name: "ctx".into(),
            when: vec![],
            transport: McpTransport::Stdio,
            command: Some("cmd".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            ..Default::default()
        };
        let caps_a = Capabilities {
            mcp: vec![server.clone()],
            ..Default::default()
        };
        let caps_b = Capabilities {
            mcp: vec![server],
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 0, caps_a), contributor("b", 1, caps_b)])
            .unwrap();
        assert_eq!(out.mcp.len(), 1, "duplicate mcp entries must be deduped");
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
        use crate::config::{LspServer, McpServer, McpTransport, SkillSource};
        use proptest::prelude::*;
        use std::collections::BTreeSet;

        fn arb_rule() -> impl Strategy<Value = PermissionRule> {
            ("[A-Za-z]{1,6}", "[a-z*]{1,6}").prop_map(|(tool, pat)| PermissionRule {
                tool,
                pattern: Some(pat),
                paths: Vec::new(),
            })
        }

        fn arb_mcp_server() -> impl Strategy<Value = McpServer> {
            (
                "[a-z]{1,6}",
                prop::collection::vec("[a-z]{1,4}", 0..3),
                proptest::option::of(0u32..3600u32),
                prop::collection::btree_map("[a-z]{1,6}", "[a-z]{1,8}", 0..3),
                prop::collection::vec("[a-z]{1,6}", 0..3),
                proptest::bool::ANY,
            )
                .prop_map(
                    |(name, tags, timeout, headers, disabled_tools, disabled)| McpServer {
                        name,
                        when: tags,
                        transport: McpTransport::Stdio,
                        command: Some("cmd".into()),
                        args: vec![],
                        env: BTreeMap::new(),
                        url: None,
                        headers,
                        disabled,
                        disabled_tools,
                        timeout,
                    },
                )
        }

        fn arb_lsp_server() -> impl Strategy<Value = LspServer> {
            (
                "[a-z]{1,6}",
                prop::collection::vec("[a-z]{1,4}", 0..3),
                prop::collection::btree_map(".[a-z]{1,4}", "[a-z]{1,8}", 0..3),
            )
                .prop_map(|(name, tags, extension_to_language)| LspServer {
                    name,
                    when: tags,
                    command: "lsp-cmd".into(),
                    extension_to_language,
                    ..Default::default()
                })
        }

        fn arb_skill_source() -> impl Strategy<Value = SkillSource> {
            ("[a-z]{1,6}", prop::collection::vec("[a-z]{1,4}", 0..3)).prop_map(|(name, tags)| {
                SkillSource {
                    name,
                    path: "/tmp/skill".into(),
                    when: tags,
                }
            })
        }

        // Contributors carrying only list fields (allow rules + plugins + mcp/lsp/skills)
        // plus a small native: map, so the scalar default_mode never forces a
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
                (
                    "[a-z]{1,4}",
                    1u8..=10,
                    prop::collection::vec(arb_rule(), 0..4),
                    prop::collection::vec("[a-z:]{1,6}", 0..4),
                ),
                (
                    prop::collection::vec(arb_mcp_server(), 0..3),
                    prop::collection::vec(arb_lsp_server(), 0..3),
                    prop::collection::vec(arb_skill_source(), 0..3),
                    prop::collection::vec(arb_engine_entry, 0..3),
                ),
            )
                .prop_map(
                    |((name, precedence, allow, plugins), (mcp, lsp, skills, engine_entries))| {
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
                                mcp,
                                lsp,
                                skills,
                                native,
                                ..Default::default()
                            },
                        }
                    },
                )
        }

        fn allow_set(caps: &Capabilities) -> BTreeSet<PermissionRule> {
            caps.permissions.allow.iter().cloned().collect()
        }

        fn plugin_set(caps: &Capabilities) -> BTreeSet<String> {
            caps.plugins.iter().cloned().collect()
        }

        fn mcp_name_set(caps: &Capabilities) -> BTreeSet<String> {
            caps.mcp.iter().map(|m| m.name.clone()).collect()
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
            // same *set* of allow rules, plugins, and mcp names (first-seen order
            // may differ, but membership is invariant).
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
                prop_assert_eq!(mcp_name_set(&forward), mcp_name_set(&backward));
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
                // mcp dedup: no two equal entries survive
                for (i, m) in out.mcp.iter().enumerate() {
                    prop_assert!(
                        !out.mcp[..i].contains(m),
                        "duplicate mcp entry at index {i}: {m:?}"
                    );
                }
            }

            // #355: env merge uses unique keys across contributors — no collisions
            // possible — so the result contains every key exactly once.
            #[test]
            fn env_unique_keys_all_survive(
                entries in prop::collection::vec(
                    ("[A-Z][A-Z0-9_]{1,6}", "[a-z]{1,8}", 1u8..=10u8),
                    0..10,
                )
            ) {
                // Build contributors with disjoint keys (index suffix forces uniqueness).
                let contribs: Vec<CapabilityContributor> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, (key, val, prec))| CapabilityContributor {
                        name: format!("c{i}"),
                        precedence: *prec,
                        capabilities: Capabilities {
                            env: std::iter::once((format!("{key}_{i}"), val.clone())).collect(),
                            ..Default::default()
                        },
                    })
                    .collect();
                let out = merge_capabilities(&contribs).unwrap();
                prop_assert_eq!(out.env.len(), contribs.len());
            }

            // #355: highest-precedence contributor wins per env key; any lower
            // contributor's value for the same key is superseded.
            // Each low contributor gets a distinct precedence (i+1) to avoid
            // same-precedence conflicts among them, which are legitimate errors.
            #[test]
            fn env_highest_precedence_wins_per_key(
                low_count in 0usize..5,
                winner_bump in 1u8..50,
            ) {
                let winner_prec = 50u8 + winner_bump;
                let mut contribs: Vec<CapabilityContributor> = (0..low_count)
                    .map(|i| CapabilityContributor {
                        name: format!("low{i}"),
                        // Distinct precedences (1-based) all below winner.
                        precedence: (i + 1) as u8,
                        capabilities: Capabilities {
                            env: std::iter::once(("KEY".to_string(), format!("low{i}"))).collect(),
                            ..Default::default()
                        },
                    })
                    .collect();
                contribs.push(CapabilityContributor {
                    name: "winner".into(),
                    precedence: winner_prec,
                    capabilities: Capabilities {
                        env: std::iter::once(("KEY".to_string(), "winner_val".to_string())).collect(),
                        ..Default::default()
                    },
                });
                let out = merge_capabilities(&contribs).unwrap();
                prop_assert_eq!(
                    out.env.get("KEY").map(String::as_str),
                    Some("winner_val"),
                    "highest-precedence contributor must win for env key KEY"
                );
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

            // resolve_host_map: disjoint keys from N contributors all survive.
            #[test]
            fn host_disjoint_keys_all_survive(
                entries in prop::collection::vec(
                    ("[a-z][a-z0-9-]{1,8}", "[a-z0-9.]{1,12}", 1u8..=10u8),
                    0..8,
                )
            ) {
                use crate::config::HostEntry;
                // Index-suffix forces unique keys per contributor.
                let contribs: Vec<CapabilityContributor> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, (name, addr, prec))| CapabilityContributor {
                        name: format!("c{i}"),
                        precedence: *prec,
                        capabilities: Capabilities {
                            host: [(
                                format!("{name}_{i}"),
                                HostEntry { addr: addr.clone() },
                            )]
                            .into_iter()
                            .collect(),
                            ..Default::default()
                        },
                    })
                    .collect();
                let out = merge_capabilities(&contribs).unwrap();
                prop_assert_eq!(out.host.len(), contribs.len());
            }

            // resolve_host_map: highest precedence wins per key, regardless of input order.
            #[test]
            fn host_highest_precedence_wins(
                low_count in 0usize..5,
                winner_bump in 1u8..50,
            ) {
                use crate::config::HostEntry;
                let winner_prec = 50u8 + winner_bump;
                let mut contribs: Vec<CapabilityContributor> = (0..low_count)
                    .map(|i| CapabilityContributor {
                        name: format!("low{i}"),
                        precedence: (i + 1) as u8,
                        capabilities: Capabilities {
                            host: [(
                                "server".to_string(),
                                HostEntry { addr: format!("low{i}.local") },
                            )]
                            .into_iter()
                            .collect(),
                            ..Default::default()
                        },
                    })
                    .collect();
                contribs.push(CapabilityContributor {
                    name: "winner".into(),
                    precedence: winner_prec,
                    capabilities: Capabilities {
                        host: [(
                            "server".to_string(),
                            HostEntry { addr: "winner.local".to_string() },
                        )]
                        .into_iter()
                        .collect(),
                        ..Default::default()
                    },
                });
                let out = merge_capabilities(&contribs).unwrap();
                prop_assert_eq!(
                    out.host.get("server").map(|e| e.addr.as_str()),
                    Some("winner.local"),
                    "highest-precedence contributor must win"
                );
            }

            // SkillSource serde roundtrip: serialize → deserialize must be identity.
            #[test]
            fn skill_source_serde_roundtrip(skill in arb_skill_source()) {
                let yaml = serde_yaml::to_string(&skill).unwrap();
                let back: SkillSource = serde_yaml::from_str(&yaml).unwrap();
                prop_assert_eq!(skill, back);
            }

            // LspServer serde roundtrip: serialize → deserialize must be identity.
            #[test]
            fn lsp_server_serde_roundtrip(lsp in arb_lsp_server()) {
                let yaml = serde_yaml::to_string(&lsp).unwrap();
                let back: LspServer = serde_yaml::from_str(&yaml).unwrap();
                prop_assert_eq!(lsp, back);
            }
        }
    }

    // #335: host entries from contributors concat by key; higher precedence wins on collision.
    #[test]
    fn host_entries_merge_by_key_precedence() {
        use crate::config::HostEntry;
        let caps_low = Capabilities {
            host: [(
                "still".to_string(),
                HostEntry {
                    addr: "still.low".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let caps_high = Capabilities {
            host: [(
                "still".to_string(),
                HostEntry {
                    addr: "still.high".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let out = merge_capabilities(&[
            contributor("low", 1, caps_low),
            contributor("high", 5, caps_high),
        ])
        .unwrap();
        assert_eq!(
            out.host["still"].addr, "still.high",
            "higher precedence must win"
        );
    }

    #[test]
    fn host_entries_disjoint_keys_union() {
        use crate::config::HostEntry;
        let caps_a = Capabilities {
            host: [(
                "server-a".to_string(),
                HostEntry {
                    addr: "a.local".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let caps_b = Capabilities {
            host: [(
                "server-b".to_string(),
                HostEntry {
                    addr: "b.local".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let out = merge_capabilities(&[contributor("a", 1, caps_a), contributor("b", 2, caps_b)])
            .unwrap();
        assert!(
            out.host.contains_key("server-a"),
            "disjoint key server-a must survive"
        );
        assert!(
            out.host.contains_key("server-b"),
            "disjoint key server-b must survive"
        );
    }

    #[test]
    fn host_same_precedence_conflict_errors() {
        use crate::config::HostEntry;
        let caps_a = Capabilities {
            host: [(
                "still".to_string(),
                HostEntry {
                    addr: "addr-a".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let caps_b = Capabilities {
            host: [(
                "still".to_string(),
                HostEntry {
                    addr: "addr-b".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let err = merge_capabilities(&[contributor("a", 3, caps_a), contributor("b", 3, caps_b)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflicting host entry"), "got: {err}");
        assert!(err.contains("still"), "got: {err}");
    }

    // #335: features.memory entries concat across contributors and dedup.
    #[test]
    fn features_memory_entries_concat_across_contributors() {
        fn mem_caps(server_host: &str, tag: &str) -> Capabilities {
            Capabilities {
                features: Some(Features {
                    memory: vec![Memory {
                        server_host: server_host.into(),
                        port: 9092,
                        listen_host: "127.0.0.1".into(),
                        when: vec![tag.into()],
                        default_topics: vec![],
                    }],
                    throttle: vec![],
                    context_mode: None,
                }),
                ..Default::default()
            }
        }
        let out = merge_capabilities(&[
            contributor("a", 1, mem_caps("still", "home")),
            contributor("b", 2, mem_caps("marks", "work")),
        ])
        .unwrap();
        let features = out.features.as_ref().expect("features must be present");
        assert_eq!(features.memory.len(), 2, "both entries must survive");
    }

    #[test]
    fn features_memory_entries_deduped() {
        fn mem_caps(server_host: &str) -> Capabilities {
            Capabilities {
                features: Some(Features {
                    memory: vec![Memory {
                        server_host: server_host.into(),
                        port: 9092,
                        listen_host: "127.0.0.1".into(),
                        when: vec!["home".into()],
                        default_topics: vec![],
                    }],
                    throttle: vec![],
                    context_mode: None,
                }),
                ..Default::default()
            }
        }
        let out = merge_capabilities(&[
            contributor("a", 1, mem_caps("still")),
            contributor("b", 2, mem_caps("still")), // identical — must dedup
        ])
        .unwrap();
        let features = out.features.as_ref().expect("features must be present");
        assert_eq!(features.memory.len(), 1, "duplicate entry must be deduped");
    }
}
