//! Resolve the active memory backend's HTTP endpoint from config + active
//! scopes, and explain why none resolved when that happens.
//!
//! Moved here from `hook_run` so `hook_run` doesn't need to depend on it —
//! `hook_run`'s own `resolve_memory_client` calls back into this module,
//! which is the expected one-way direction (this module never depends on
//! `hook_run`).

use anyhow::Context as _;
use llmenv_mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, resolve_mcps};

/// What memory-backend resolution found for the active scope.
///
/// Replaces the `Option<String>` that collapsed five distinguishable states
/// into `None` (#1131/#1132/#1140): a caller could not tell a project that
/// simply declares no memory from one whose only memory-carrying bundle is
/// suppressed by `disable_bundles`, or one whose declared entry is simply
/// gated on a tag that isn't active right now. The fifth state — a failed
/// bundle merge — is an `Err` from [`memory_url`] rather than a variant here,
/// because a backend may well be configured and merely unparseable: that is a
/// failure, not an absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEndpoint {
    /// The memory backend resolved to this HTTP URL, carrying the active
    /// `features.memory` entry's configured `wakeup_max_tokens` (#1216,
    /// `None` if unset).
    Active {
        url: String,
        wakeup_max_tokens: Option<u32>,
    },
    /// No bundle fired for the active scopes and no top-level `features.memory`
    /// entry matched — nothing could have supplied a backend.
    NoBundlesFired,
    /// Bundles fired, but neither they nor the top-level config declare a
    /// `features.memory` entry active for these tags. `skipped_bundles` names
    /// firing bundles that `build_bundle_refs` dropped — for having no content
    /// directory, or for a rejected/unsafe name — so their `bundle.yaml` was
    /// never read (#1133/#1142).
    NotDeclared { skipped_bundles: Vec<String> },
    /// `features.memory` is supplied only by these bundles, which the active
    /// scopes suppress via `disable_bundles` (#194).
    SuppressedByDisableBundles(Vec<String>),
    /// A top-level or firing-bundle `features.memory` entry exists for these
    /// `server_host`s, but none of their `when` tags intersect the active
    /// scope — distinct from [`Self::NoBundlesFired`] (nothing declared at
    /// all) and [`Self::NotDeclared`] (declared by a bundle whose content
    /// never loaded). `resolve_mcps`'s `0 => {}` arm drops a tag-inactive
    /// entry silently; this variant is how `classify_missing_memory`
    /// recovers that information on the failure path (#1140).
    TagInactive { server_hosts: Vec<String> },
}

impl MemoryEndpoint {
    /// The resolved URL, or an error naming why no backend is active.
    ///
    /// # Errors
    /// Every non-[`MemoryEndpoint::Active`] variant, rendered as the
    /// user-facing reason it is inactive.
    pub(crate) fn into_url(self) -> anyhow::Result<String> {
        const PREFIX: &str = "no memory backend active for this scope";
        match self {
            Self::Active { url, .. } => Ok(url),
            Self::NoBundlesFired => Err(anyhow::anyhow!(
                "{PREFIX}: no bundles fired and config.yaml declares no features.memory"
            )),
            Self::NotDeclared { skipped_bundles } if skipped_bundles.is_empty() => {
                Err(anyhow::anyhow!(
                    "{PREFIX}: no active bundle or config.yaml declares features.memory"
                ))
            }
            Self::NotDeclared { skipped_bundles } => Err(anyhow::anyhow!(
                "{PREFIX}: bundle(s) {} fired but were skipped while loading bundle \
                 content — either no content directory under the config dir's \
                 bundles/, or the bundle name was rejected (e.g. a traversal/absolute \
                 path) — so any features.memory they declare was never loaded",
                skipped_bundles.join(", ")
            )),
            Self::SuppressedByDisableBundles(names) => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory is supplied only by bundle(s) {}, which \
                 this project turns off via disable_bundles",
                names.join(", ")
            )),
            Self::TagInactive { server_hosts } => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory declares server_host(s) {}, but none of \
                 their `when` tags are in the active scope",
                server_hosts.join(", ")
            )),
        }
    }

    /// The active entry's configured wake-up token budget (#1216). `None`
    /// for every non-[`MemoryEndpoint::Active`] variant, and for `Active`
    /// itself when `features.memory[].wakeup_max_tokens` is unset.
    pub(crate) fn wakeup_max_tokens(&self) -> Option<u32> {
        match self {
            Self::Active {
                wakeup_max_tokens, ..
            } => *wakeup_max_tokens,
            _ => None,
        }
    }

    /// Consume into `(url, wakeup_max_tokens)`, erroring exactly as
    /// [`Self::into_url`] — `into_url` itself keeps its existing signature
    /// since it has several other callers that don't need the token budget.
    pub(crate) fn into_url_and_wakeup_max_tokens(self) -> anyhow::Result<(String, Option<u32>)> {
        let wakeup_max_tokens = self.wakeup_max_tokens();
        Ok((self.into_url()?, wakeup_max_tokens))
    }
}

/// Find the resolved memory backend's HTTP URL for the active tags, or the
/// reason none resolved.
///
/// Mirrors the `build_manifest` merge strategy: top-level config memory is
/// combined with bundle-contributed memory entries so a daemon declared only
/// in a `bundle.yaml` is reachable from lifecycle hooks.
///
/// # Errors
/// A bundle merge failure (#1132) or an unresolvable MCP/memory declaration.
pub(crate) fn memory_url(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<MemoryEndpoint> {
    let top_memory = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();

    // Collect bundle-contributed memory and host entries. Bundle selection goes
    // through `bundle_select::firing_bundles` — the same selector `build_manifest` uses —
    // so `disable_bundles` suppression can't drift between hook-run's live
    // resolution and the materialized manifest (#1125). The `tag_filter` must
    // stay `None`: it exists for the CLI's `--tag` flag, and narrowing endpoint
    // resolution by one tag would drop the memory backend for a live session.
    let firing = crate::bundle_select::firing_bundles(&config.bundle, active, None);

    let bundle_refs = crate::bundle_select::build_bundle_refs(config_dir, active, &firing);
    let (bundle_memory, bundle_host) = resolve_bundle_memory_host(config, &bundle_refs)?;

    let mut all_memory: Vec<crate::config::Memory> = top_memory
        .iter()
        .chain(bundle_memory.iter())
        .cloned()
        .collect();
    llmenv_util::dedup(&mut all_memory);

    // Merged host: bundle contributions first, top-level overwrites (same as build_manifest).
    let mut all_host = bundle_host;
    for (k, v) in &config.host {
        all_host.insert(k.clone(), v.clone());
    }

    // Full `active.tags` (not `non_project_tags()`) on purpose: this resolves
    // the memory backend for the *live* hook-run session, which is legitimately
    // project-aware — unlike `build_manifest`'s static host-cache render, which
    // must exclude project-only tags (#696/#979). Do not "align" this with
    // build_manifest's host_tags; that would break recall in project scopes.
    let resolved = resolve_mcps(&config.mcp, &all_memory, &all_host, &active.tags)
        .map_err(|e| annotate_resolve_error(e, config, config_dir, active))?;
    let matched = resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => {
            Some((url, m.wakeup_max_tokens))
        }
        _ => None,
    });
    Ok(match matched {
        Some((url, wakeup_max_tokens)) => MemoryEndpoint::Active {
            url,
            wakeup_max_tokens,
        },
        None => classify_missing_memory(
            config,
            config_dir,
            active,
            &firing,
            &bundle_refs,
            &all_memory,
        ),
    })
}

/// Explain why no memory endpoint resolved (#1131).
///
/// Only reached once resolution has already come up empty, so the extra
/// `bundle.yaml` reads it does to attribute a cause stay off the hot path that
/// every hook event takes.
///
/// `all_memory` is the same merged top-level + bundle-contributed list
/// `resolve_mcps` was called with. By construction every entry in it has a
/// `when` that doesn't intersect `active.tags`: an intersecting entry would
/// have either resolved (`MemoryEndpoint::Active`) or, if more than one
/// intersected, made `resolve_mcps` return `Err` before this function is ever
/// reached. So a non-empty `all_memory` here means "declared, but tag-inactive"
/// (#1140), not "declared and active."
///
/// Priority order (checked in this order because each is a strictly more
/// actionable cause than the next): `disable_bundles` suppression, then a
/// firing bundle `build_bundle_refs` couldn't load (a real misconfiguration —
/// its `features.memory`, if any, was never even read), then a declared but
/// tag-inactive entry (often intentional — e.g. a `when` scoped to a network
/// the user isn't on right now), then no bundles firing at all, then the
/// fully benign case of bundles that fired, loaded, and simply declare no
/// memory.
fn classify_missing_memory(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    firing: &[&crate::config::Bundle],
    bundle_refs: &[crate::merge::BundleRef],
    all_memory: &[crate::config::Memory],
) -> MemoryEndpoint {
    let suppressed = suppressed_memory_bundles(config, config_dir, active);
    if !suppressed.is_empty() {
        return MemoryEndpoint::SuppressedByDisableBundles(suppressed);
    }
    let loaded: std::collections::HashSet<&str> =
        bundle_refs.iter().map(|r| r.name.as_str()).collect();
    let skipped_bundles: Vec<String> = firing
        .iter()
        .map(|b| b.name.as_str())
        .filter(|n| !loaded.contains(n))
        .map(str::to_owned)
        .collect();
    if !skipped_bundles.is_empty() {
        return MemoryEndpoint::NotDeclared { skipped_bundles };
    }
    if !all_memory.is_empty() {
        return MemoryEndpoint::TagInactive {
            server_hosts: all_memory
                .iter()
                .map(|m| m.server_host.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
    }
    if firing.is_empty() {
        return MemoryEndpoint::NoBundlesFired;
    }
    MemoryEndpoint::NotDeclared {
        skipped_bundles: Vec::new(),
    }
}

/// Name `disable_bundles` in a resolution failure when that is what withdrew
/// the `host:` entry the memory block points at (#1131). Without it the user
/// sees a `server_host` they can read in their own `config.yaml` and nothing
/// connecting it to the bundle they turned off.
fn annotate_resolve_error(
    err: llmenv_mcp::resolve::ResolveError,
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Error {
    if let llmenv_mcp::resolve::ResolveError::MemoryUnknownServerHost(host) = &err {
        let suppliers: Vec<String> = suppressed_bundle_capabilities(config, config_dir, active)
            .into_iter()
            .filter(|(_, caps)| caps.host.contains_key(host))
            .map(|(name, _)| name)
            .collect();
        if !suppliers.is_empty() {
            return anyhow::anyhow!(
                "failed to resolve MCP servers: {err} — it is declared in bundle(s) \
                 {}, which this project turns off via disable_bundles",
                suppliers.join(", ")
            );
        }
    }
    anyhow::anyhow!("failed to resolve MCP servers: {err}")
}

/// `bundle.yaml` capabilities of every bundle the active scopes suppress via
/// `disable_bundles` (#194) but that would otherwise have fired, in config
/// declaration order.
///
/// Diagnostic-only, read on the failure path so llmenv can say *why* no
/// endpoint resolved rather than only *that* none did. A suppressed bundle
/// whose own `bundle.yaml` is missing or unreadable contributes nothing here —
/// a failed explanation must not replace the failure being explained.
fn suppressed_bundle_capabilities(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> Vec<(String, crate::config::Capabilities)> {
    let disabled = crate::bundle_select::marker_disabled_bundle_names(active);
    if disabled.is_empty() {
        return Vec::new();
    }
    let manually_enabled = crate::bundle_select::marker_enabled_bundle_names(active);
    let would_fire: Vec<&crate::config::Bundle> = config
        .bundle
        .iter()
        .filter(|b| disabled.contains(&b.name))
        .filter(|b| crate::bundle_select::tag_or_marker_selected(b, active, &manually_enabled))
        .collect();
    crate::bundle_select::build_bundle_refs(config_dir, active, &would_fire)
        .into_iter()
        .filter_map(|r| {
            crate::merge::read_bundle_yaml(&r.path, &r.name)
                .ok()
                .flatten()
                .map(|caps| (r.name, caps))
        })
        .collect()
}

/// Names of [`suppressed_bundle_capabilities`]' bundles that would supply a
/// tag-active `features.memory` entry if `disable_bundles` didn't suppress
/// them — the "would this disabled bundle have supplied memory" filter,
/// shared so `classify_missing_memory` and
/// `cli::doctor::memory_orphaned_by_disable_bundles` can't drift on it
/// (#1141). Tag-active, not merely present (#1140): a suppressed bundle whose
/// only `features.memory` entry is itself gated on an inactive tag wouldn't
/// have supplied memory even if re-enabled, so it must not be named as the
/// cause.
///
/// `pub(crate)`: called by `cli::doctor`, whose orphaned-memory check needs
/// the same filtered list this diagnostic does.
pub(crate) fn suppressed_memory_bundles(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> Vec<String> {
    suppressed_bundle_capabilities(config, config_dir, active)
        .into_iter()
        .filter(|(_, caps)| {
            caps.features.as_ref().is_some_and(|f| {
                f.memory
                    .iter()
                    .any(|m| llmenv_mcp::resolve::memory_is_tag_active(m, &active.tags))
            })
        })
        .map(|(name, _)| name)
        .collect()
}

/// Resolve the bundle-only memory/host slice for `bundle_refs` (#920).
///
/// Tries the disk-persisted cache first — written by `regenerate`/`export`
/// (`build_manifest` in `materialize`) — keyed on `merge_signature`, which is
/// cheap to recompute here (reads only each firing bundle's `bundle.yaml`,
/// not the full merge). A hit skips the full `merge()` call entirely; a miss
/// (no artifact yet, or the signature changed because config or bundle
/// content changed since the last regenerate) falls back to a live merge.
///
/// Both failure modes are reported rather than swallowed (#1132). A signature
/// failure only costs the optimization, so it logs and falls through to the
/// live merge; a live-merge failure means a backend may well be configured and
/// unparseable, so it propagates — the lifecycle hook's fail-soft wrapper turns
/// it into `llmenv: memory <event> skipped: {e}`, which names the real cause
/// instead of the misleading "no memory backend active for this scope".
fn resolve_bundle_memory_host(
    config: &crate::config::Config,
    bundle_refs: &[crate::merge::BundleRef],
) -> anyhow::Result<(
    Vec<crate::config::Memory>,
    std::collections::BTreeMap<String, crate::config::HostEntry>,
)> {
    if bundle_refs.is_empty() {
        return Ok((Vec::new(), std::collections::BTreeMap::new()));
    }

    let disk_hit = crate::merge::merge_signature(&config.capabilities, &config.native, bundle_refs)
        .inspect_err(|e| {
            // `error!`, not `warn!` (#1139): the process's own `EnvFilter` is
            // ERROR-only by default, same as the three detached children this
            // diff fixes for the identical reason — a `warn!` here would be
            // silently dropped, same as theirs was.
            tracing::error!("failed to compute merge signature for cache lookup: {e}");
        })
        .ok()
        .and_then(|key| {
            let cache_root =
                std::path::PathBuf::from(crate::paths::expand_tilde(&config.cache.cache_dir));
            crate::materialize::merge_cache::read_if_matching(&cache_root, &key)
        });
    if let Some(hit) = disk_hit {
        return Ok(hit);
    }

    let merged = crate::merge::merge(&config.capabilities, &config.native, bundle_refs)
        .context("failed to merge bundle capabilities for memory-backend resolution")?;
    let mem = merged
        .capabilities
        .features
        .map(|f| f.memory)
        .unwrap_or_default();
    Ok((mem, merged.capabilities.host))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn valid_name() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{1,24}"
    }

    // ===== #1143: classify_missing_memory() classification over combinatorial state =====

    /// A minimal but fully-populated `Memory` entry naming `server_host` —
    /// `classify_missing_memory` only reads `server_host` off `all_memory`,
    /// but the struct has no `Default` shorthand for the rest.
    fn memory_with_host(server_host: &str) -> crate::config::Memory {
        crate::config::Memory {
            server_host: server_host.to_string(),
            port: 7878,
            listen_host: "127.0.0.1".into(),
            when: vec![],
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: std::collections::BTreeMap::new(),
            retention: None,
            auto_prune: false,
            consolidation: None,
            mcp_permissions: None,
            wakeup_max_tokens: None,
        }
    }

    proptest! {
        // A non-empty `all_memory` yields `TagInactive` naming every entry's
        // `server_host` (deduped) whenever nothing is suppressed and every
        // firing bundle loaded cleanly — regardless of how many bundles
        // fired. `TagInactive` outranks `NoBundlesFired` (this test) but is
        // itself outranked by a skipped firing bundle (the test below).
        #[test]
        fn prop_classify_missing_memory_tag_inactive_wins_regardless_of_firing(
            server_hosts in proptest::collection::vec(valid_name(), 1..5),
            firing_names in proptest::collection::vec(valid_name(), 0..5),
        ) {
            let config = crate::config::Config::default();
            let config_dir = std::path::Path::new("/nonexistent");
            let active = crate::scope::ActiveScopes::default();
            let bundles: Vec<crate::config::Bundle> = firing_names
                .iter()
                .map(|n| crate::config::Bundle { name: n.clone(), when: vec![] })
                .collect();
            let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
            // Every firing bundle "loads" successfully (a matching `BundleRef`
            // for each name), so `skipped_bundles` stays empty and this
            // isolates the `TagInactive`-vs-`NoBundlesFired` priority — a
            // skipped bundle outranks `TagInactive` (see the test below), so
            // it must not leak into this one.
            let bundle_refs: Vec<crate::merge::BundleRef> = firing_names
                .iter()
                .map(|n| crate::merge::BundleRef {
                    name: n.clone(),
                    path: std::path::PathBuf::new(),
                    precedence: 0,
                })
                .collect();
            let all_memory: Vec<crate::config::Memory> =
                server_hosts.iter().map(|h| memory_with_host(h)).collect();

            let result = classify_missing_memory(
                &config,
                config_dir,
                &active,
                &firing,
                &bundle_refs,
                &all_memory,
            );
            // `classify_missing_memory` dedups `server_hosts` (two entries can
            // legitimately share a `server_host`), so compare against the same
            // deduped/sorted form rather than `server_hosts` as generated.
            let expected: Vec<String> = server_hosts
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            prop_assert_eq!(
                result,
                MemoryEndpoint::TagInactive { server_hosts: expected }
            );
        }

        // A firing bundle `build_bundle_refs` couldn't load outranks a
        // declared-but-tag-inactive entry: the skipped bundle is a real
        // misconfiguration (its own `features.memory`, if any, was never
        // read), while a tag-inactive entry is often intentional.
        #[test]
        fn prop_classify_missing_memory_skipped_bundle_outranks_tag_inactive(
            server_hosts in proptest::collection::vec(valid_name(), 1..5),
            skipped_name in valid_name(),
        ) {
            let config = crate::config::Config::default();
            let config_dir = std::path::Path::new("/nonexistent");
            let active = crate::scope::ActiveScopes::default();
            let bundles = [crate::config::Bundle { name: skipped_name.clone(), when: vec![] }];
            let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
            let all_memory: Vec<crate::config::Memory> =
                server_hosts.iter().map(|h| memory_with_host(h)).collect();

            // No bundle_refs at all: `skipped_name` is firing but unloaded.
            let result =
                classify_missing_memory(&config, config_dir, &active, &firing, &[], &all_memory);
            prop_assert_eq!(
                result,
                MemoryEndpoint::NotDeclared { skipped_bundles: vec![skipped_name] }
            );
        }

        // With `all_memory` empty and nothing suppressed: `NoBundlesFired` iff
        // firing is empty; otherwise `NotDeclared`'s `skipped_bundles` is
        // exactly `firing` filtered down to the names `bundle_refs` didn't
        // load — matching production's `firing \ loaded` computation
        // (duplicates and order included, since production filters the
        // `firing` list itself rather than deduplicating first).
        #[test]
        fn prop_classify_missing_memory_skipped_bundles_is_firing_minus_loaded(
            firing_with_loaded in proptest::collection::vec((valid_name(), any::<bool>()), 0..6),
        ) {
            let config = crate::config::Config::default();
            let config_dir = std::path::Path::new("/nonexistent");
            let active = crate::scope::ActiveScopes::default();
            let firing_names: Vec<String> =
                firing_with_loaded.iter().map(|(n, _)| n.clone()).collect();
            let bundles: Vec<crate::config::Bundle> = firing_names
                .iter()
                .map(|n| crate::config::Bundle { name: n.clone(), when: vec![] })
                .collect();
            let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
            let loaded_names: std::collections::HashSet<&str> = firing_with_loaded
                .iter()
                .filter(|(_, keep)| *keep)
                .map(|(n, _)| n.as_str())
                .collect();
            let bundle_refs: Vec<crate::merge::BundleRef> = loaded_names
                .iter()
                .map(|n| crate::merge::BundleRef {
                    name: (*n).to_string(),
                    path: std::path::PathBuf::new(),
                    precedence: 0,
                })
                .collect();

            let result =
                classify_missing_memory(&config, config_dir, &active, &firing, &bundle_refs, &[]);

            if firing_names.is_empty() {
                prop_assert_eq!(result, MemoryEndpoint::NoBundlesFired);
            } else {
                let MemoryEndpoint::NotDeclared { skipped_bundles } = result else {
                    return Err(proptest::test_runner::TestCaseError::fail(format!(
                        "expected NotDeclared for non-empty firing, got {result:?}"
                    )));
                };
                // Independent membership invariants rather than recomputing
                // production's `firing \ loaded` formula (#1141 pre-pr-review
                // finding): every skipped name is a firing bundle that wasn't
                // loaded, and every unloaded firing bundle is reported.
                for name in &skipped_bundles {
                    prop_assert!(
                        firing_names.contains(name),
                        "skipped name {name:?} must be one of the firing bundles"
                    );
                    prop_assert!(
                        !loaded_names.contains(name.as_str()),
                        "skipped name {name:?} must not be among the loaded bundle_refs"
                    );
                }
                for name in &firing_names {
                    if !loaded_names.contains(name.as_str()) {
                        prop_assert!(
                            skipped_bundles.contains(name),
                            "unloaded firing bundle {name:?} must appear in skipped_bundles"
                        );
                    }
                }
            }
        }
    }
}
