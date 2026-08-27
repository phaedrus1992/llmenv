//! Bundle/marker selection logic shared by `cli` (rendering, doctor) and
//! `hook_run`/`memory` (live memory-endpoint resolution) — factored out so
//! callers' notion of "which bundles are active" can't drift apart (#1141,
//! #1125).

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use crate::config::Bundle;
use crate::merge::BundleRef;
use crate::scope::ActiveScopes;

/// Resolve `firing` bundles to their on-disk content directories, ranked by
/// the scope tier that selected them (network > host > user > content >
/// project, highest tier wins on precedence). A bundle with no content
/// directory is skipped with a warning (tag-only bundle, or a deleted
/// directory).
pub(crate) fn build_bundle_refs(
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
) -> Vec<BundleRef> {
    const PRECEDENCE: &[&str] = &["network", "host", "user", "content", "project"];

    let bundles_dir = config_dir.join("bundles");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut refs: Vec<BundleRef> = Vec::new();

    let push_ref =
        |name: &str, precedence: u8, refs: &mut Vec<BundleRef>, seen: &mut BTreeSet<String>| {
            if seen.contains(name) {
                return;
            }
            if crate::paths::is_unsafe_join_target(name) {
                tracing::warn!("rejecting bundle name with traversal/absolute path: {name}");
                return;
            }
            let path = bundles_dir.join(name);
            if !path.exists() {
                // Stays at `warn!` (which the default `EnvFilter` drops)
                // because a content-less bundle is a documented, valid
                // configuration and this runs on every hook event — promoting
                // it would make a legitimate setup log an error continuously.
                // The one place it silently changed an outcome, memory-endpoint
                // resolution, names the skipped bundles itself instead
                // (`hook_run::MemoryEndpoint::NotDeclared`, #1133).
                tracing::warn!(
                    "bundle '{}' has no content directory at {}; \
                     skipping (tag-only bundle, or missing/deleted directory)",
                    name,
                    path.display()
                );
                return;
            }
            seen.insert(name.to_owned());
            refs.push(BundleRef {
                name: name.to_owned(),
                path,
                precedence,
            });
        };

    for (tier, kind) in PRECEDENCE.iter().enumerate() {
        // Earlier tiers (network) outrank later ones (project) for scalar
        // capability resolution, matching the placement-precedence order.
        // `tier` ranges 0..PRECEDENCE.len() (4), so the rank is 1..=4 — always
        // in u8 range. try_from over `as` so a future PRECEDENCE growth past 255
        // tiers fails loudly instead of silently wrapping.
        let precedence = u8::try_from(PRECEDENCE.len() - tier).unwrap_or(u8::MAX);
        // Tags emitted by scopes of this kind.
        let kind_tags: BTreeSet<&str> = active
            .scopes
            .iter()
            .filter(|s| s.kind == *kind)
            .flat_map(|s| s.tags.iter().map(String::as_str))
            .collect();
        for bundle in firing {
            if bundle.when.iter().any(|t| kind_tags.contains(t.as_str())) {
                push_ref(&bundle.name, precedence, &mut refs, &mut seen);
            }
        }
    }
    // Any firing bundle not already placed (shouldn't happen — every firing
    // bundle has at least one tag in active.tags — but defensive). Lowest rank.
    for bundle in firing {
        push_ref(&bundle.name, 0, &mut refs, &mut seen);
    }
    refs
}

/// Bundle names referenced via marker `enable_bundles` in the currently
/// active scopes.
pub(crate) fn marker_enabled_bundle_names(active: &ActiveScopes) -> HashSet<String> {
    active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().cloned())
        .collect()
}

/// Bundle names any active scope disables via marker `disable_bundles`
/// (#194). Currently populated only by the project marker; see
/// `ActiveScope::disable_bundles`.
pub(crate) fn marker_disabled_bundle_names(active: &ActiveScopes) -> HashSet<String> {
    active
        .scopes
        .iter()
        .flat_map(|s| s.disable_bundles.iter().cloned())
        .collect()
}

/// Whether `bundle` would be selected by tag intersection or explicit
/// `enable_bundles`, ignoring `disable_bundles` entirely — the shared "would
/// this fire" core. `firing_bundles` layers the `disable_bundles` subtraction
/// (and its own, CLI-only `--tag` narrowing) on top; `hook_run::
/// suppressed_bundle_capabilities` needs the inverse (only bundles
/// disable_bundles turns off) and reimplemented this same predicate
/// separately until #1141 factored it out here so the two selection rules
/// can't drift apart. Deliberately 3-param, not 4: the `--tag` flag is a
/// display-only narrowing orthogonal to "would this fire," not part of the
/// rule itself, so it stays a separate filter stage in `firing_bundles`
/// rather than a dummy `None` every non-CLI caller has to pass.
pub(crate) fn tag_or_marker_selected(
    bundle: &Bundle,
    active: &ActiveScopes,
    manually_enabled: &HashSet<String>,
) -> bool {
    bundle.when.iter().any(|bt| active.tags.contains(bt)) || manually_enabled.contains(&bundle.name)
}

/// Compute the bundles that fire for `active`: tag intersection OR
/// `enable_bundles`, minus anything any scope disables via `disable_bundles`
/// (#194) — disable always wins, including within the same scope that also
/// enables it (there's no cross-scope precedence question today since
/// `enable_bundles`/`disable_bundles` are only populated for project scopes,
/// the highest-precedence scope kind; a disable from project always beats a
/// lower scope's tag-firing or enable simply by being the final subtraction).
/// `tag_filter` (the CLI `--tag` flag) additionally gates a bundle's `when`
/// list when present. Shared by every call site that needs "what bundles are
/// actually selected" so the suppression rule can't drift between them.
pub(crate) fn firing_bundles<'a>(
    bundles: &'a [Bundle],
    active: &ActiveScopes,
    tag_filter: Option<&str>,
) -> Vec<&'a Bundle> {
    let manually_enabled = marker_enabled_bundle_names(active);
    let disabled = marker_disabled_bundle_names(active);
    bundles
        .iter()
        .filter(|b| !disabled.contains(&b.name))
        .filter(|b| tag_filter.is_none_or(|t| b.when.iter().any(|w| w == t)))
        .filter(|b| tag_or_marker_selected(b, active, &manually_enabled))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn bundle(name: &str, when: &[&str]) -> Bundle {
        Bundle {
            name: name.to_string(),
            when: when.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn active_scope(
        kind: &'static str,
        tags: &[&str],
        enable_bundles: &[&str],
        disable_bundles: &[&str],
    ) -> crate::scope::ActiveScope {
        crate::scope::ActiveScope {
            id: kind.to_string(),
            kind,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            project_root: None,
            enable_bundles: enable_bundles.iter().map(|s| s.to_string()).collect(),
            disable_bundles: disable_bundles.iter().map(|s| s.to_string()).collect(),
            name: None,
            description: None,
            unknown_fields: vec![],
        }
    }

    fn active(scopes: Vec<crate::scope::ActiveScope>) -> ActiveScopes {
        let tags = scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
        ActiveScopes {
            scopes,
            tags,
            ..Default::default()
        }
    }

    #[test]
    fn firing_bundles_tag_matched_bundle_fires() {
        let bundles = vec![bundle("rust-dev", &["rust"])];
        let active = active(vec![active_scope("user", &["rust"], &[], &[])]);
        let firing = firing_bundles(&bundles, &active, None);
        assert_eq!(
            firing.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["rust-dev"]
        );
    }

    #[test]
    fn firing_bundles_manually_enabled_bundle_fires_without_matching_tag() {
        let bundles = vec![bundle("github-issues", &[])];
        let active = active(vec![active_scope("project", &[], &["github-issues"], &[])]);
        let firing = firing_bundles(&bundles, &active, None);
        assert_eq!(
            firing.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["github-issues"]
        );
    }

    // ===== Tests for build_bundle_refs precedence (#845) =====

    /// Creates an empty content directory for `name` under `config_dir/bundles/`
    /// — `build_bundle_refs` silently drops any bundle without one.
    fn with_bundle_dir(config_dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(config_dir.join("bundles").join(name)).expect("test");
    }

    #[test]
    fn build_bundle_refs_orders_by_scope_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["net-b", "host-b", "user-b", "content-b", "project-b"] {
            with_bundle_dir(tmp.path(), name);
        }
        let bundles = vec![
            bundle("net-b", &["t-net"]),
            bundle("host-b", &["t-host"]),
            bundle("user-b", &["t-user"]),
            bundle("content-b", &["t-content"]),
            bundle("project-b", &["t-project"]),
        ];
        let active = active(vec![
            active_scope("network", &["t-net"], &[], &[]),
            active_scope("host", &["t-host"], &[], &[]),
            active_scope("user", &["t-user"], &[], &[]),
            active_scope("content", &["t-content"], &[], &[]),
            active_scope("project", &["t-project"], &[], &[]),
        ]);
        let firing = firing_bundles(&bundles, &active, None);
        let refs = build_bundle_refs(tmp.path(), &active, &firing);
        let ranks: std::collections::BTreeMap<&str, u8> = refs
            .iter()
            .map(|r| (r.name.as_str(), r.precedence))
            .collect();
        assert!(
            ranks["net-b"] > ranks["host-b"]
                && ranks["host-b"] > ranks["user-b"]
                && ranks["user-b"] > ranks["content-b"]
                && ranks["content-b"] > ranks["project-b"],
            "expected network > host > user > content > project, got {ranks:?}"
        );
    }

    #[test]
    fn build_bundle_refs_content_scope_is_not_lowest_rank() {
        // Regression for #845: content used to fall through PRECEDENCE's
        // catch-all (rank 0) because it wasn't listed at all, ranking below
        // every other scope kind regardless of specificity.
        let tmp = tempfile::tempdir().unwrap();
        with_bundle_dir(tmp.path(), "content-only");
        let bundles = vec![bundle("content-only", &["t-content"])];
        let active = active(vec![active_scope("content", &["t-content"], &[], &[])]);
        let firing = firing_bundles(&bundles, &active, None);
        let refs = build_bundle_refs(tmp.path(), &active, &firing);
        assert_eq!(refs.len(), 1);
        assert!(
            refs[0].precedence > 0,
            "a content-only-fired bundle must not land in the catch-all lowest rank: {refs:?}"
        );
    }

    #[test]
    fn build_bundle_refs_unmatched_enable_bundles_falls_to_catch_all() {
        // A bundle fired only via `enable_bundles` (no scope's tags cover it)
        // has no tier to place into and lands in the defensive catch-all.
        let tmp = tempfile::tempdir().unwrap();
        with_bundle_dir(tmp.path(), "manual-b");
        let bundles = vec![bundle("manual-b", &[])];
        let active = active(vec![active_scope("project", &[], &["manual-b"], &[])]);
        let firing = firing_bundles(&bundles, &active, None);
        let refs = build_bundle_refs(tmp.path(), &active, &firing);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].precedence, 0);
    }

    #[test]
    fn firing_bundles_disable_suppresses_tag_matched_bundle() {
        // #194 motivating example: a lower-precedence scope's tag turns on
        // "yaks"; the project scope disables it.
        let bundles = vec![bundle("yaks", &["task-tracking"])];
        let active = active(vec![
            active_scope("user", &["task-tracking"], &[], &[]),
            active_scope("project", &[], &[], &["yaks"]),
        ]);
        let firing = firing_bundles(&bundles, &active, None);
        assert!(
            firing.is_empty(),
            "disable must suppress tag-firing: {firing:?}"
        );
    }

    #[test]
    fn firing_bundles_disable_suppresses_manually_enabled_bundle() {
        let bundles = vec![bundle("yaks", &[])];
        let active = active(vec![active_scope("project", &[], &["yaks"], &["yaks"])]);
        let firing = firing_bundles(&bundles, &active, None);
        assert!(
            firing.is_empty(),
            "same-scope disable must beat same-scope enable: {firing:?}"
        );
    }

    #[test]
    fn firing_bundles_disable_does_not_affect_unrelated_bundles() {
        let bundles = vec![
            bundle("yaks", &["task-tracking"]),
            bundle("rust-dev", &["rust"]),
        ];
        let active = active(vec![
            active_scope("user", &["task-tracking", "rust"], &[], &[]),
            active_scope("project", &[], &[], &["yaks"]),
        ]);
        let firing = firing_bundles(&bundles, &active, None);
        assert_eq!(
            firing.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["rust-dev"]
        );
    }

    #[test]
    fn firing_bundles_tag_filter_still_applies_alongside_disable() {
        let bundles = vec![bundle("a", &["x"]), bundle("b", &["y"])];
        let active = active(vec![active_scope("user", &["x", "y"], &[], &[])]);
        let firing = firing_bundles(&bundles, &active, Some("x"));
        assert_eq!(
            firing.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
    }
}
