use crate::config::Config;
use crate::paths;
use anyhow::Context;
use std::collections::HashSet;

/// Effective value of a token-efficiency env var: the process environment
/// wins if set (matches what Claude Code will actually see if it inherited
/// the shell), otherwise fall back to `native.claude_code.env` in the
/// resolved config — a var declared there lands in settings.json's own `env`
/// block, which Claude Code applies to itself independent of the shell that
/// launched it, so it counts as "set" even when the shell never exported it.
fn effective_token_efficiency_var(
    native_claude_env: Option<&serde_yaml::Value>,
    key: &str,
) -> Option<String> {
    if let Ok(val) = std::env::var(key) {
        return Some(val);
    }
    let value = native_claude_env?.get(key)?;
    value
        .as_str()
        .map(String::from)
        .or_else(|| value.as_bool().map(|b| b.to_string()))
        .or_else(|| value.as_i64().map(|n| n.to_string()))
}

pub(super) fn run_doctor_token_efficiency(
    use_color: bool,
    pass: &str,
    warn: &str,
    cm_enabled: bool,
    native_claude_env: Option<&serde_yaml::Value>,
) {
    let info = super::doctor_info(use_color);
    eprintln!();
    eprintln!("Token-efficiency checks:");
    let get = |key: &str| effective_token_efficiency_var(native_claude_env, key);

    match get("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE") {
        Some(val) => match val.parse::<u32>() {
            Ok(pct) if pct <= 70 => eprintln!("{pass} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct}"),
            Ok(pct) => eprintln!(
                "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct} (recommend ≤70 for PreCompact cleanup)"
            ),
            Err(_) => {
                eprintln!("{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE has invalid (non-numeric) value")
            }
        },
        None => eprintln!(
            "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE not set (recommend 50 for PreCompact headroom)"
        ),
    }

    match get("BASH_MAX_OUTPUT_LENGTH").map(|v| v.parse::<u64>()) {
        Some(Ok(n)) => eprintln!("{pass} BASH_MAX_OUTPUT_LENGTH={n}"),
        Some(Err(_)) => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH has invalid (non-numeric) value"),
        None => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH not set (recommend 10000)"),
    }

    match get("MAX_MCP_OUTPUT_TOKENS").map(|v| v.parse::<u64>()) {
        Some(Ok(n)) => eprintln!("{pass} MAX_MCP_OUTPUT_TOKENS={n}"),
        Some(Err(_)) => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS has invalid (non-numeric) value"),
        None => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS not set (recommend 10000)"),
    }

    match get("ENABLE_PROMPT_CACHING_1H") {
        Some(val) if val.eq_ignore_ascii_case("true") || val == "1" => {
            eprintln!("{pass} ENABLE_PROMPT_CACHING_1H=true (1h cache TTL enabled)")
        }
        Some(_) => {
            eprintln!("{warn} ENABLE_PROMPT_CACHING_1H has unexpected value (recommend true)")
        }
        None => {
            eprintln!("{warn} ENABLE_PROMPT_CACHING_1H not set (recommend true for 1h cache reuse)")
        }
    }

    match get("CLAUDE_CODE_SUBAGENT_MODEL") {
        Some(_) => eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL is set"),
        None => {
            eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL not set (default: claude-sonnet-4-6)")
        }
    }

    if cm_enabled {
        eprintln!("{pass} context-mode built-in feature enabled (token-efficiency)");
    } else {
        eprintln!(
            "{info} context-mode not enabled \
             (set features.context_mode.enabled: true for built-in context saving)"
        );
    }
}

pub(super) fn run_doctor(gc: bool, all: bool, use_color: bool) -> anyhow::Result<()> {
    let pass = super::doctor_pass(use_color);
    let warn = super::doctor_warning(use_color);

    eprintln!("Running llmenv doctor...");

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let cm_enabled = config.context_mode_enabled();
    eprintln!("{pass} Configuration loaded from {}", config_path.display());

    // Check that config parses
    eprintln!("{pass} Config is valid YAML");

    // Check cache directory is writable
    let cache_dir = super::expand_tilde(&config.cache.cache_dir)?;
    std::fs::create_dir_all(&cache_dir).context("cache directory not writable")?;
    eprintln!(
        "{pass} Cache directory is writable: {}",
        cache_dir.display()
    );

    // Report the active cache layout so `doctor` explains the folder shape on disk.
    match config.cache.hashing {
        crate::config::HashingMode::Loose => {
            eprintln!("{pass} Cache hashing: loose (folder: <shape>)");
        }
        crate::config::HashingMode::Normal => {
            eprintln!(
                "{pass} Cache hashing: normal (folder: {}/<shape>)",
                crate::materialize::cache::version_mm()
            );
        }
        crate::config::HashingMode::Strict => {
            eprintln!("{pass} Cache hashing: strict (content-addressed folders)");
        }
    }

    // Check for version skew across all registered adapters
    let skew_relevant = !matches!(config.cache.hashing, crate::config::HashingMode::Loose);
    if skew_relevant {
        for adapter in crate::adapter::registered_adapters() {
            let adapter_cache = cache_dir.join(adapter.name());
            if let Ok(entries) = std::fs::read_dir(&adapter_cache) {
                let mut cached_versions: Vec<String> = Vec::new();
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if dir_name.ends_with(".tmp") {
                        continue;
                    }
                    let version = match dir_name.rsplit_once('-') {
                        Some((prefix, tail)) if super::is_content_hash(tail) => prefix.to_string(),
                        _ => dir_name.to_string(),
                    };
                    cached_versions.push(version);
                }
                cached_versions.sort();
                cached_versions.dedup();
                let version_folder = crate::materialize::cache::version_mm();
                let current_built = |v: &String| v == super::VERSION_TAG || *v == version_folder;
                if !cached_versions.is_empty() {
                    let cached_versions_str = cached_versions.join(", ");
                    if !cached_versions.iter().any(current_built) {
                        eprintln!(
                            "{warn} {} version skew detected: running llmenv {} but cache has versions [{}]",
                            adapter.name(),
                            super::VERSION_TAG,
                            cached_versions_str
                        );
                        eprintln!("{warn}   → Fix: cargo install --path . --force");
                    }
                }
            }
        }
    }

    // Check git remote is reachable
    let config_dir = paths::config_dir()?;
    if super::is_git_repo(&config_dir) {
        match super::check_git_remote(&config_dir) {
            Ok(remote) => {
                let safe_url = crate::git::sanitize_git_url(&remote);
                eprintln!("{pass} Git remote reachable: {}", safe_url);
            }
            Err(e) => eprintln!("{warn} Git remote check failed: {}", e),
        }
    } else {
        eprintln!("{warn} Config directory is not a git repo");
    }

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    // Cross-engine hook compatibility (#543 follow-up): name any hook that will
    // be silently skipped when materializing for an installed adapter with a
    // narrower supported-hook-event set (e.g. Crush only supports PreToolUse).
    // Only checks adapters actually on PATH — an adapter you don't have
    // installed skipping a hook it could never run isn't worth flagging.
    let doctor_firing = super::firing_bundles(&config.bundle, &active, None);
    let doctor_manifest =
        super::build_manifest(&config, &config_dir, &active, &doctor_firing, false)?;
    if let Some((manifest, _)) = &doctor_manifest {
        for adapter in super::installed_adapters(&config) {
            let supported = adapter.supported_hook_events();
            for hook in &manifest.capabilities.hooks {
                if !supported.contains(&hook.event.as_str()) {
                    eprintln!(
                        "{warn} hook event '{}' is not supported by the {} adapter — \
                         it will be skipped, not materialized. Supported events: {}",
                        hook.event,
                        adapter.name(),
                        supported.join(", ")
                    );
                }
            }
        }
    }
    // Resolved native.claude_code.env, for the token-efficiency checks below
    // to treat as equally "set" alongside the process environment (#543 follow-up).
    let native_claude_env = doctor_manifest
        .as_ref()
        .and_then(|(manifest, _)| manifest.native.get("claude_code"))
        .and_then(|v| v.get("env"));

    if all {
        // Orphan detection
        let mut emitted = super::all_emitted_tags(&config);
        emitted.extend(active.tags.iter().cloned());
        let consumed = super::all_consumed_tags(&config);
        let marker_enabled = super::marker_enabled_bundle_names(&active);

        let mut orphan_count: usize = 0;
        for s in &config.scope.network {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope network:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }
        for s in &config.scope.host {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope host:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }
        for s in &config.scope.user {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope user:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }

        let configured_bundle_names: std::collections::HashSet<&str> =
            config.bundle.iter().map(|b| b.name.as_str()).collect();
        for scope in &active.scopes {
            if scope.kind != "project" {
                continue;
            }
            for field in &scope.unknown_fields {
                eprintln!("{warn} unknown field in .llmenv.yaml: {field}");
                orphan_count += 1;
            }
            for bundle_name in &scope.enable_bundles {
                if !configured_bundle_names.contains(bundle_name.as_str()) {
                    eprintln!(
                        "{warn} .llmenv.yaml enable_bundles references unknown bundle: {bundle_name}"
                    );
                    orphan_count += 1;
                }
            }
            for bundle_name in &scope.disable_bundles {
                if !configured_bundle_names.contains(bundle_name.as_str()) {
                    eprintln!(
                        "{warn} .llmenv.yaml disable_bundles references unknown bundle: {bundle_name}"
                    );
                    orphan_count += 1;
                }
                // #194: same-scope enable+disable is contradictory intent —
                // disable wins at runtime, but flag it so the user notices
                // the enable_bundles entry is dead.
                if scope.enable_bundles.contains(bundle_name) {
                    eprintln!(
                        "{warn} .llmenv.yaml enables and disables the same bundle: {bundle_name} \
                         (disable wins; the enable_bundles entry has no effect)"
                    );
                    orphan_count += 1;
                }
            }
        }

        for b in &config.bundle {
            let has_emitted_tag = b.when.iter().any(|t| emitted.contains(t));
            let looks_marker = super::looks_marker_driven(&b.name, b);
            if !has_emitted_tag && !marker_enabled.contains(&b.name) && !looks_marker {
                eprintln!("{warn} orphan bundle {}: no scope emits its tags", b.name);
                orphan_count += 1;
            }
        }

        for m in &config.mcp {
            let has_emitted_tag = m.when.iter().any(|t| emitted.contains(t));
            let looks_marker = m.when.iter().any(|t| super::tag_looks_marker_sourced(t));
            if !has_emitted_tag && !looks_marker {
                eprintln!("{warn} orphan mcp {}: no scope emits its tags", m.name);
                orphan_count += 1;
            }
        }

        // Build merged host table for server_host checks
        let doctor_firing: Vec<_> = super::firing_bundles(&config.bundle, &active, None);

        let doctor_bundle_caps = {
            let refs = super::build_bundle_refs(&config_dir, &active, &doctor_firing);
            if refs.is_empty() {
                crate::config::Capabilities::default()
            } else {
                crate::merge::merge(&config.capabilities, &config.native, &refs)
                    .context("failed to merge bundle capabilities for orphan check")?
                    .capabilities
            }
        };

        let mut merged_host_for_doctor = doctor_bundle_caps.host.clone();
        for (k, v) in &config.host {
            merged_host_for_doctor.insert(k.clone(), v.clone());
        }

        // Check top-level memory entries
        if let Some(features) = &config.features {
            for mem in &features.memory {
                let has_emitted_tag = mem.when.iter().any(|t| emitted.contains(t));
                if !has_emitted_tag {
                    eprintln!(
                        "{warn} orphan memory (server_host '{}'): no scope emits its tags",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
                if !merged_host_for_doctor.contains_key(&mem.server_host) {
                    eprintln!(
                        "{warn} memory: server_host '{}' has no entry in the host: table",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
            }
        }

        // Check bundle-contributed memory entries
        if let Some(features) = &doctor_bundle_caps.features {
            for mem in &features.memory {
                let has_emitted_tag = mem.when.iter().any(|t| emitted.contains(t));
                if !has_emitted_tag {
                    eprintln!(
                        "{warn} orphan bundle memory (server_host '{}'): no scope emits its tags",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
                if !merged_host_for_doctor.contains_key(&mem.server_host) {
                    eprintln!(
                        "{warn} bundle memory: server_host '{}' has no entry in host: table",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
            }
        }

        // Plugin orphans
        {
            use crate::config::split_plugin_ref;

            let mut referenceable: HashSet<&str> = HashSet::new();
            for c in &config.plugin_collection {
                let selectable = c.when.iter().any(|t| emitted.contains(t));
                if !selectable {
                    eprintln!(
                        "{warn} orphan plugin-collection {}: no scope emits its tags",
                        c.name
                    );
                    orphan_count += 1;
                }
                if selectable {
                    referenceable.extend(
                        c.plugins
                            .iter()
                            .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m)),
                    );
                }
            }
            for m in &config.marketplace {
                // When context-mode is enabled as a built-in feature the user
                // need not declare it in a plugin-collection — the built-in
                // injection covers it. Suppress the false orphan warning.
                let builtin_exempt =
                    cm_enabled && m.name == crate::config::CONTEXT_MODE_MARKETPLACE;
                if !builtin_exempt && !referenceable.contains(m.name.as_str()) {
                    eprintln!(
                        "{warn} orphan marketplace {}: no selectable plugin references it",
                        m.name
                    );
                    orphan_count += 1;
                }
            }
        }

        // Tag orphans
        let mut tag_universe: HashSet<String> = HashSet::new();
        tag_universe.extend(emitted.iter().cloned());
        tag_universe.extend(consumed.iter().cloned());
        tag_universe.extend(active.tags.iter().cloned());
        let mut tag_orphans: Vec<String> = tag_universe
            .into_iter()
            .filter(|t| {
                let emitted_anywhere = emitted.contains(t)
                    || active.tags.contains(t)
                    || super::tag_looks_marker_sourced(t);
                let consumed_anywhere = consumed.contains(t);
                !(emitted_anywhere && consumed_anywhere)
            })
            .collect();
        tag_orphans.sort();
        for t in &tag_orphans {
            let emitted_anywhere = emitted.contains(t)
                || active.tags.contains(t)
                || super::tag_looks_marker_sourced(t);
            let reason = if !emitted_anywhere {
                "no scope emits it"
            } else {
                "no bundle consumes it"
            };
            eprintln!("{warn} orphan tag {}: {}", t, reason);
            orphan_count += 1;
        }

        if orphan_count == 0 {
            eprintln!("{pass} No orphan scopes/tags/bundles/plugins");
        } else {
            eprintln!("{warn} Found {} orphan item(s)", orphan_count);
        }
    } // end if all

    // Lint for ${CLAUDE_PLUGIN_ROOT} in non-plugin hooks
    for hook in &config.capabilities.hooks {
        if let Some(cmd) = &hook.handler.command
            && cmd.contains("${CLAUDE_PLUGIN_ROOT}")
        {
            eprintln!(
                "{warn} Hook command references ${{CLAUDE_PLUGIN_ROOT}} but runs in top-level settings.json: {}",
                cmd
            );
            eprintln!(
                "{warn}   → ${{CLAUDE_PLUGIN_ROOT}} only works in plugin-scoped hooks/hooks.json files"
            );
            eprintln!("{warn}   → Move or rewrite this hook in your config or bundle YAML");
        }
    }

    run_doctor_token_efficiency(use_color, &pass, &warn, cm_enabled, native_claude_env);

    eprintln!("{pass} Doctor check complete.");

    if gc {
        eprintln!("Running garbage collection...");
        match std::fs::metadata(&cache_dir) {
            Ok(meta) => {
                if meta.permissions().readonly() {
                    eprintln!("{warn} GC failed: cache directory is read-only");
                } else {
                    let cache_retention_hours = config.cache.cache_retention_hours.unwrap_or(168);
                    let retention = std::time::Duration::from_secs(cache_retention_hours * 3600);
                    match crate::materialize::cache::gc(&cache_dir, retention) {
                        Ok(report) => {
                            eprintln!(
                                "{pass} GC complete: removed {} entries, kept {}",
                                report.removed.len(),
                                report.kept
                            );
                        }
                        Err(e) => eprintln!("{warn} GC failed: {}", e),
                    }
                }
            }
            Err(e) => eprintln!("{warn} GC failed to stat cache directory: {}", e),
        }
    }

    Ok(())
}
