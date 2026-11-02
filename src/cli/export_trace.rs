//! Core export logic for `llmenv export` (#3009).
//!
//! This module holds the export pipeline: load config, detect scope, materialize
//! adapters, and emit environment variables. The `run_export` entry point in
//! `src/cli/mod.rs` handles CLI setup and delegates here.

use super::{
    build_manifest, ensure_local_memory_proxy, firing_bundles, installed_adapters,
    validate_var_name, validate_var_value, warn_dead_config,
};
use crate::config::{Bundle, Config};
use crate::scope::ActiveScopes;
use crate::scope::matcher::Env;
use anyhow::Context;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

/// Run the export pipeline: load config, detect scope, materialize adapters,
/// and emit environment variables.
///
/// `scope` and `tag` gate which bundles fire. `explain` adds comments.
/// `compress` controls bundle compression.
pub(crate) fn run_export_core(
    scope: Option<String>,
    tag: Option<String>,
    explain: bool,
    compress: bool,
) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let config_path = crate::paths::config_path()?;
    let config = crate::hook_run::load_cached_config(&config_path)?;
    let config_dir = crate::paths::config_dir()?;

    let t_config = Instant::now();
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
    let t_env = Instant::now();
    let active = crate::scope::evaluate(&config, &env);
    let t_active = Instant::now();

    ensure_local_memory_proxy(
        &config,
        &config_dir,
        &active,
        crate::mcp::proxy_ops::SpawnSource::Export,
    );
    pull_if_due(&config, &config_dir);

    // A bundle fires when either:
    //   - one of its tags is in the active tag set (normal tag-based firing), OR
    //   - an active scope manually enables it by name via `enable_bundles`
    //     in a marker file,
    // minus anything a scope disables via `disable_bundles` (#194). The
    // optional --tag filter still gates the tag/enable path.
    let mut vars = BTreeMap::new();

    // When --scope <id> is passed, restrict to only that scope's tags so
    // `llmenv export --scope office` exports only what fires from the
    // "office" network scope, not all active scopes.  Scope match conditions
    // (network gateway/ssid/cidr, host hostname, user user, project path/
    // marker) are already evaluated by evaluate() above — this simply
    // narrows the result to one scope's contribution.
    let (firing, use_active) =
        select_firing(&config, &active, &env, scope.as_deref(), tag.as_deref());

    // Merge + materialize the agent config directory for each installed adapter
    // and collect env vars. PATH-gating skips adapters whose binary is absent so
    // a machine without a given tool sees zero behavior change. (#502)
    let cache_dir_root = PathBuf::from(crate::paths::expand_tilde(&config.cache.cache_dir)?);

    // Build the merged manifest once before the adapter loop — the manifest
    // is adapter-independent, so re-building it per adapter is wasted work (#708).
    // Each adapter gets its own clone since materialize_from_manifest mutates it
    // (tag filtering, compression, etc.).
    let t_manifest = Instant::now();
    let shared_manifest = build_manifest(&config, &config_dir, &use_active, &firing, false)?;

    warn_dead_config(
        &config,
        shared_manifest.as_ref().map(|(m, _)| m),
        "warning:",
    );

    let mut failed_adapters: Vec<&str> = Vec::new();
    let mut any_adapter_eligible = false;
    for adapter in installed_adapters(&config) {
        any_adapter_eligible = true;

        let result = match &shared_manifest {
            Some((m, cache_root)) => {
                let mut cloned = m.clone();
                crate::cli::materialize_from_manifest(
                    adapter.as_ref(),
                    &mut cloned,
                    cache_root,
                    &use_active,
                    &config,
                    compress,
                )
            }
            None => {
                crate::throttle::store_active_throttle(None)?;
                Ok(None)
            }
        };

        match result {
            Ok(Some((ref cache_path, ref extra_vars))) => {
                tracing::debug!(
                    adapter = adapter.name(),
                    "materialized agent config at {}",
                    cache_path.display()
                );
                for (k, v) in extra_vars {
                    vars.insert(k.clone(), v.clone());
                }
                // Auth sync: detect in-session login changes and refresh the
                // stable cache. Non-fatal — export must not fail on auth errors.
                sync_adapter_auth(cache_path, &cache_dir_root.join(adapter.name()));
            }
            Ok(None) => {
                tracing::debug!(
                    adapter = adapter.name(),
                    "no bundle content directories — skipping materialize"
                );
            }
            // Non-fatal (#543): one adapter's materialize failure (e.g. Crush
            // rejecting an unsupported hook event) must not take down every
            // other adapter's env vars. A machine with `crush` on PATH but an
            // incompatible bundle would otherwise lose Claude Code output too,
            // breaking the PATH-gating promise that unrelated setups see zero
            // change.
            Err(e) => {
                failed_adapters.push(adapter.name());
                eprintln!(
                    "warning: {} adapter materialization failed (skipping): {e:#}",
                    adapter.name()
                );
            }
        }
    }

    let t_adapters = Instant::now();

    report_adapter_outcome(vars.is_empty(), &failed_adapters, any_adapter_eligible)?;

    // Introspection env: comma-separated, deterministic order. Scopes get
    // a `<kind>:<id>` prefix so the kind is visible without re-running
    // `llmenv scope ls`. Tags come from a BTreeSet (already sorted); bundles
    // are emitted in declaration order.
    let scopes_csv = use_active
        .scopes
        .iter()
        .map(|s| format!("{}:{}", s.kind, s.id))
        .collect::<Vec<_>>()
        .join(",");
    let tags_csv = use_active
        .tags
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    let bundles_csv = firing
        .iter()
        .map(|b| b.name.clone())
        .collect::<Vec<_>>()
        .join(",");
    vars.insert("LLMENV_ACTIVE_SCOPES".into(), scopes_csv);
    vars.insert("LLMENV_ACTIVE_TAGS".into(), tags_csv);
    vars.insert("LLMENV_ACTIVE_BUNDLES".into(), bundles_csv);

    // LLMENV_ACTIVE_PROJECT / LLMENV_PROJECT_ROOT: deepest matched project
    // scope wins (most specific for nested project layouts). Both vars are
    // skipped when no project scope is active.
    let winning_project = use_active
        .scopes
        .iter()
        .filter(|s| s.kind == "project")
        .filter_map(|s| s.project_root.as_ref().map(|r| (s, r)))
        .max_by_key(|(_, r)| r.as_os_str().len());
    if let Some((scope, root)) = winning_project {
        vars.insert("LLMENV_ACTIVE_PROJECT".into(), scope.id.clone());
        vars.insert(
            "LLMENV_PROJECT_ROOT".into(),
            root.to_string_lossy().into_owned(),
        );
    }

    // ICM context chunk: encode active tags/bundles for tag-scoped memory
    // retrieval across projects. Agents can store memory under keyword
    // "llmenv-tag:<tag>" and it will be retrieved in any scope with that tag.
    let bundles_for_icm = firing.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
    let icm_chunk = crate::icm::generate_context_chunk(&use_active, &bundles_for_icm);
    vars.insert("LLMENV_ICM_CONTEXT".into(), icm_chunk);

    // Store tag/bundle mappings for SessionStart hook retrieval
    if let Err(e) = crate::icm::store_tag_memory(&use_active, &bundles_for_icm) {
        tracing::warn!(
            "failed to store ICM tag memory, so bundle recall may miss this tag: {e}. Fix: check the ICM backend with `llmenv doctor`."
        );
    }

    // Auto-prune: TTL-based memory retention pass after materialization (#270)
    crate::memory::auto_prune::auto_prune_if_enabled(&config, &config_dir, &use_active);

    let bundle_list = firing
        .iter()
        .map(|b| b.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    print_exports(&vars, explain.then_some(bundle_list.as_str()))?;

    crate::export_trace::emit_phase_trace(
        "export",
        t0,
        &[
            ("config_load_us", Some(t_config)),
            ("detect_env_us", Some(t_env)),
            ("evaluate_active_us", Some(t_active)),
            ("build_manifest_us", Some(t_manifest)),
            ("materialize_adapters_us", Some(t_adapters)),
            ("render_vars_us", Some(Instant::now())),
        ],
    );

    Ok(())
}

/// The bundles that fire, and the scopes that the materialize step uses. With `--scope`, only that
/// scope's tags count. An unknown scope id fires nothing for it, and a warning says so.
fn select_firing<'a>(
    config: &'a Config,
    active: &ActiveScopes,
    env: &Env,
    scope: Option<&str>,
    tag: Option<&str>,
) -> (Vec<&'a Bundle>, ActiveScopes) {
    let Some(scope_id) = scope else {
        let firing = firing_bundles(&config.bundle, active, tag);
        return (firing, active.clone());
    };
    let filtered_scopes = active
        .scopes
        .iter()
        .filter(|s| s.id == scope_id)
        .cloned()
        .collect::<Vec<_>>();
    if filtered_scopes.is_empty() {
        eprintln!(
            "warning: scope '{scope_id}' not active in current environment \
             — no bundles will fire for this scope"
        );
        let firing = firing_bundles(&config.bundle, active, tag);
        return (firing, active.clone());
    }
    let mut filtered_tags: BTreeSet<String> = filtered_scopes
        .iter()
        .flat_map(|s| s.tags.iter().cloned())
        .collect();
    if !env.os.is_empty() {
        filtered_tags.insert(env.os.clone());
    }
    filtered_tags.extend(active.extra_tags.iter().cloned());

    let filtered = ActiveScopes {
        scopes: filtered_scopes,
        tags: filtered_tags,
        extra_tags: active.extra_tags.clone(),
    };
    let firing = firing_bundles(&config.bundle, &filtered, tag);
    (firing, filtered)
}

/// Refresh the stable auth cache from a materialized folder's login state. Non-fatal: export must
/// not fail on an auth error.
fn sync_adapter_auth(cache_path: &Path, adapter_root: &Path) {
    match crate::materialize::manifest::CacheManifest::read(cache_path) {
        Ok(Some(mut manifest)) => crate::auth::detect::sync_auth_on_export(
            cache_path,
            adapter_root,
            &mut manifest,
            crate::auth::credentials::Backend::detect(),
        ),
        // No manifest yet, so there is no login state to sync. A corrupt manifest is logged
        // inside CacheManifest::read.
        Ok(None) => {}
        Err(e) => tracing::warn!(
            "auth sync skipped: could not read cache manifest at {}: {e}",
            cache_path.display()
        ),
    }
}

/// The `export` lines for `vars`. Every variable is validated before any line is returned, so one
/// bad value fails the whole export and a shell never sources part of it (#543). With `bundle_list`,
/// each line first names its source, so an explained export shows which bundles fired.
fn export_lines(
    vars: &BTreeMap<String, String>,
    bundle_list: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::with_capacity(vars.len() * 2);
    for (key, value) in vars {
        validate_var_name(key).with_context(|| format!("variable '{key}'"))?;
        validate_var_value(value).with_context(|| format!("variable '{key}': invalid value"))?;
        if let Some(bundle_list) = bundle_list {
            if key.starts_with("LLMENV_") {
                lines.push("# source: llmenv introspection".to_string());
            } else {
                lines.push(format!("# source: adapter (bundles: {bundle_list})"));
            }
        }
        lines.push(format!(
            "export {}={}",
            key,
            crate::util::single_quote(value)
        ));
    }
    Ok(lines)
}

/// Print the `export` lines for `vars`. Nothing prints unless every variable is valid.
fn print_exports(vars: &BTreeMap<String, String>, bundle_list: Option<&str>) -> anyhow::Result<()> {
    for line in export_lines(vars, bundle_list)? {
        println!("{line}");
    }
    Ok(())
}

/// Throttled pull: fetch and pull the config repository once the sync interval has passed. It is
/// skipped when remote sync is off, for example while 1Password is locked. A failed pull is a
/// notice for the agent, and export still runs on the config it has.
fn pull_if_due(config: &Config, config_dir: &Path) {
    if !config.cache.remote_sync {
        return;
    }
    let interval_secs = config.cache.sync_interval_minutes * 60;
    let state_dir = match crate::paths::state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the throttled pull is skipped, because the state folder is unknown \
                 ({e:#}). Fix: check $HOME and LLMENV_STATE_DIR."
            ));
            return;
        }
    };
    let interval = std::time::Duration::from_secs(interval_secs);
    if let Err(e) = crate::sync::maybe_pull(config_dir, &state_dir, interval) {
        crate::agent_notice::record_logged(format!(
            "llmenv: the throttled pull of the config repository failed, so this export uses the \
             config it has ({e:#}). Fix: check the remote and the network, then run `llmenv sync`."
        ));
    }
}

/// Report the adapter outcome. A missing adapter binary is a warning. Every adapter failing with no
/// env vars is an error, so `eval` does not source nothing and exit 0 (#543). A partial failure
/// stays exit 0, because this runs on every prompt through the shell hook (#1346).
fn report_adapter_outcome(
    no_vars: bool,
    failed: &[&str],
    any_eligible: bool,
) -> anyhow::Result<()> {
    if !any_eligible {
        eprintln!(
            "warning: no registered adapter binary found on PATH (expected: {})",
            crate::adapter::registered_adapters()
                .iter()
                .map(|a| a.binary_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if no_vars && !failed.is_empty() {
        anyhow::bail!("all adapter materializations failed — no env vars to export");
    }
    if !failed.is_empty() {
        eprintln!(
            "warning: exported environment is missing output from: {}. Run `llmenv \
             regenerate` to see the full error.",
            failed.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
