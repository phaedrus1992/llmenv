use crate::config::{Bundle, Config};
use crate::paths;
use crate::plugins::cache;
use anyhow::Context;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// Returns bundle names whose directory does not exist under `bundles_dir`.
pub(super) fn bundles_with_missing_dirs<'a>(
    bundles: &'a [Bundle],
    bundles_dir: &Path,
) -> Vec<&'a str> {
    bundles
        .iter()
        .filter(|b| !bundles_dir.join(&b.name).is_dir())
        .map(|b| b.name.as_str())
        .collect()
}

/// Returns marketplace names defined in `config` that no plugin collection references.
pub(super) fn unused_marketplaces(config: &Config) -> Vec<&str> {
    use crate::config::split_plugin_ref;
    let referenced: HashSet<&str> = config
        .plugin_collection
        .iter()
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p))
        .map(|(m, _)| m)
        .collect();
    config
        .marketplace
        .iter()
        .filter(|m| !referenced.contains(m.name.as_str()))
        .map(|m| m.name.as_str())
        .collect()
}

/// Check whether a host address string is a loopback / local-only address.
fn is_local_addr(addr: &str) -> bool {
    matches!(addr, "localhost" | "0.0.0.0" | "::" | "::0")
        || addr
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether any memory block references a remote (non-local) server_host.
/// Returns `true` when a `server_host` maps to a non-local address in the host
/// table, or when the host table has no entry for it (assume remote).
fn has_remote_memory_host(config: &Config) -> bool {
    config.features.as_ref().is_some_and(|f| {
        f.memory.iter().any(|mem| {
            config
                .host
                .get(&mem.server_host)
                .is_none_or(|h| !is_local_addr(&h.addr))
        })
    })
}

/// Check that external tools referenced by the active config are available on
/// `$PATH`. Printed to stderr using the doctor pass/fail/info helpers inline
/// with the rest of `llmenv doctor`.
fn run_doctor_tool_availability(use_color: bool, config: &Config) {
    let pass = super::doctor_pass(use_color);
    let fail = super::doctor_fail(use_color);
    let info = super::doctor_info(use_color);

    eprintln!();
    eprintln!("Tool-availability checks:");

    let has_memory = config
        .features
        .as_ref()
        .is_some_and(|f| !f.memory.is_empty());

    // icm — required when features.memory has entries
    // mcp-proxy or uvx — required when any memory server_host is remote
    if has_memory {
        if crate::adapter::binary_on_path("icm") {
            eprintln!("{pass} icm found on PATH");
        } else {
            eprintln!("{fail} icm not found on PATH (required when features.memory is configured)");
        }

        if has_remote_memory_host(config) {
            if crate::adapter::binary_on_path("mcp-proxy") || crate::adapter::binary_on_path("uvx")
            {
                eprintln!("{pass} mcp-proxy or uvx found on PATH (remote memory server_host)");
            } else {
                eprintln!(
                    "{fail} neither mcp-proxy nor uvx on PATH \
                     (remote memory server_host requires one for TCP proxying)"
                );
            }
        }
    }

    // claude — required when claude_code engine is not disabled
    let claude_disabled = config
        .disabled_engines
        .iter()
        .any(|e| e.eq_ignore_ascii_case("claude_code"));
    if !claude_disabled {
        if crate::adapter::binary_on_path("claude") {
            eprintln!("{pass} claude found on PATH");
        } else {
            eprintln!(
                "{fail} claude not found on PATH \
                 (claude_code engine is not disabled, but the `claude` binary is missing)"
            );
        }
    }

    // crush, opencode — always optional
    for bin in &["crush", "opencode"] {
        if crate::adapter::binary_on_path(bin) {
            eprintln!("{pass} {bin} found on PATH");
        } else {
            eprintln!("{info} {bin} not found on PATH (optional engine)");
        }
    }
}

/// Returns `native_permissions` keys that don't match any configured MCP server
/// or known engine adapter name. The ICM MCP server (`"icm"`) is always present.
pub(super) fn orphan_native_permission_keys(config: &Config) -> Vec<&str> {
    let known_mcps: HashSet<&str> = config
        .mcp
        .iter()
        .map(|m| m.name.as_str())
        .chain(std::iter::once("icm"))
        .collect();
    const ENGINE_NAMES: &[&str] = &["claude_code"];
    config
        .capabilities
        .native_permissions
        .keys()
        .filter(|k| !known_mcps.contains(k.as_str()) && !ENGINE_NAMES.contains(&k.as_str()))
        .map(|k| k.as_str())
        .collect()
}

/// Tool names Claude Code's `hook.matcher` regex can legitimately target — it
/// matches only the tool name, never a file path or extension. Kept local to
/// this check since no shared canonical list exists elsewhere in the codebase.
const CLAUDE_CODE_TOOL_NAMES: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "Bash",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "Task",
];

/// Whether `matcher` is a bare tool name, `^Name$`, or a `^(A|B|C)$`
/// alternation over `CLAUDE_CODE_TOOL_NAMES`.
fn matches_known_tool_pattern(matcher: &str) -> bool {
    let prefix_stripped = matcher.strip_prefix('^').unwrap_or(matcher);
    let inner = prefix_stripped.strip_suffix('$').unwrap_or(prefix_stripped);
    let inner = inner
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(inner);
    inner
        .split('|')
        .all(|part| CLAUDE_CODE_TOOL_NAMES.contains(&part))
}

/// Whether `matcher` is shaped like a file-extension glob (`*.rs`, `**/*.py`)
/// or a bare extension (`.rs`) rather than a tool-name pattern.
fn looks_like_file_glob(matcher: &str) -> bool {
    if let Some(ext) = matcher.strip_prefix('.') {
        return !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric());
    }
    matcher.match_indices("*.").any(|(idx, _)| {
        matcher[idx + 2..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Returns `"{event} (matcher: '{matcher}')"` for each hook whose matcher is
/// shaped like a file-extension glob instead of a Claude Code tool-name
/// pattern — a common misconfiguration, since Claude Code matches
/// `hook.matcher` against tool name only, never file path.
pub(super) fn hooks_with_glob_like_matchers(config: &Config) -> Vec<String> {
    config
        .capabilities
        .hooks
        .iter()
        .filter_map(|hook| {
            let matcher = hook.matcher.as_deref()?;
            (looks_like_file_glob(matcher) && !matches_known_tool_pattern(matcher))
                .then(|| format!("{} (matcher: '{}')", hook.event, matcher))
        })
        .collect()
}

pub(super) fn run_doctor(gc: bool, all: bool, use_color: bool) -> anyhow::Result<()> {
    let pass = super::doctor_pass(use_color);
    let warn = super::doctor_warning(use_color);
    let info = super::doctor_info(use_color);

    eprintln!("Running llmenv doctor...");

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let cm_enabled = config.context_mode_enabled();
    eprintln!("{pass} Configuration loaded from {}", config_path.display());

    // Check that config parses
    eprintln!("{pass} Config is valid YAML");

    // Structural validation: bundle directories, marketplace references, permission grants
    let config_dir = paths::config_dir()?;
    let bundles_dir = config_dir.join("bundles");

    for name in bundles_with_missing_dirs(&config.bundle, &bundles_dir) {
        eprintln!(
            "{info} Bundle '{name}' declared but directory does not exist at {}",
            bundles_dir.join(name).display(),
        );
    }

    for name in unused_marketplaces(&config) {
        eprintln!(
            "{warn} Marketplace '{name}' is defined but not referenced by any plugin collection",
        );
    }

    for key in orphan_native_permission_keys(&config) {
        eprintln!(
            "{warn} native_permissions key '{key}' does not match any configured MCP server, engine, or adapter",
        );
    }

    for hit in hooks_with_glob_like_matchers(&config) {
        eprintln!(
            "{warn} hook {hit} looks like a file-extension glob, but Claude Code matches \
             hook.matcher against tool name only, never file path — use a `scope.content` \
             glob to gate the hook's bundle by file type instead",
        );
    }

    // Check cache directory is writable
    let cache_dir = PathBuf::from(crate::paths::expand_tilde(&config.cache.cache_dir));
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
            } else {
                tracing::warn!(
                    "failed to read adapter cache directory {:?} for version skew check",
                    adapter_cache,
                );
            }
        }
    }

    // Check git remote is reachable
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

    run_doctor_tool_availability(use_color, &config);

    // When context-mode is enabled, verify the marketplace clone exists so
    // inject_context_mode can actually resolve the plugin. A missing clone is
    // the most common reason the auto-wire looks correct in config but fails
    // at materialize time.
    if cm_enabled {
        let mkt_name = crate::config::CONTEXT_MODE_MARKETPLACE;
        let mkt_path = crate::plugins::cache::marketplace_path(&cache_dir, mkt_name);
        if !mkt_path.exists() {
            eprintln!(
                "{warn} context-mode marketplace '{mkt_name}' not synced — \
                 run `llmenv plugin-sync` so the auto-wire can find it"
            );
        } else {
            eprintln!("{pass} context-mode marketplace '{mkt_name}' synced and ready");
        }
    }

    // Verify pinned marketplaces: when a marketplace source includes a `#ref`
    // pin, the checked-out HEAD should match that pinned ref. Use `^{commit}`
    // dereferencing so annotated tags don't false-positive (#695).
    for m in &config.marketplace {
        let (_, pinned_ref) = cache::split_source_ref(&m.source);
        let Some(pinned_ref) = pinned_ref else {
            continue;
        };
        let mkt_path = cache::marketplace_path(&cache_dir, &m.name);
        if !mkt_path.join(".git").exists() {
            continue;
        }
        let Some(head) = cache::git_head(&mkt_path) else {
            // Clone exists but HEAD can't be resolved — let the `plugin-sync`
            // / materialize paths report the broken clone.
            continue;
        };
        let Some(pinned_sha) = cache::git_peeled_ref(&mkt_path, pinned_ref) else {
            eprintln!(
                "{warn} marketplace '{}' pinned to '{}' but that ref cannot be \
                 resolved in the local clone — run `llmenv plugin-sync` to repair",
                m.name, pinned_ref,
            );
            continue;
        };
        if head != pinned_sha {
            eprintln!(
                "{warn} marketplace '{}' pinned to '{}': HEAD ({}) does not match \
                 the pinned ref's commit ({}) — run `llmenv plugin-sync` to repair",
                m.name,
                pinned_ref,
                &head[..head.len().min(7)],
                &pinned_sha[..pinned_sha.len().min(7)],
            );
        }
    }

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
                    match crate::materialize::cache::gc(&cache_dir, retention, config.cache.hashing)
                    {
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
mod tests {
    use super::*;
    use crate::config::{
        Bundle, Capabilities, Features, Hook, HostEntry, Marketplace, McpServer, McpTransport,
        Memory, NativePermissionRules, PluginCollection,
    };
    use std::collections::BTreeMap;

    // -- bundles_with_missing_dirs --

    #[test]
    fn bundles_missing_none_when_all_dirs_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("home")).unwrap();
        std::fs::create_dir_all(bundles_dir.join("work")).unwrap();

        let bundles = vec![
            Bundle {
                name: "home".into(),
                when: vec!["local".into()],
            },
            Bundle {
                name: "work".into(),
                when: vec!["office".into()],
            },
        ];
        let missing = bundles_with_missing_dirs(&bundles, &bundles_dir);
        assert!(missing.is_empty(), "expected empty: {missing:?}");
    }

    #[test]
    fn bundles_missing_reports_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("existing")).unwrap();

        let bundles = vec![
            Bundle {
                name: "existing".into(),
                when: vec!["x".into()],
            },
            Bundle {
                name: "missing".into(),
                when: vec!["y".into()],
            },
        ];
        let mut missing = bundles_with_missing_dirs(&bundles, &bundles_dir);
        missing.sort_unstable();
        assert_eq!(missing, vec!["missing"]);
    }

    // -- unused_marketplaces --

    #[test]
    fn unused_marketplaces_none_when_all_referenced() {
        let config = Config {
            marketplace: vec![Marketplace {
                name: "official".into(),
                source: "https://example.com".into(),
            }],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: vec!["official:some-plugin".into()],
            }],
            ..Config::default()
        };
        let unused = unused_marketplaces(&config);
        assert!(unused.is_empty(), "expected empty: {unused:?}");
    }

    #[test]
    fn unused_marketplaces_reports_unreferenced() {
        let config = Config {
            marketplace: vec![
                Marketplace {
                    name: "used".into(),
                    source: "https://a.com".into(),
                },
                Marketplace {
                    name: "unused".into(),
                    source: "https://b.com".into(),
                },
            ],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: vec!["used:plugin-a".into()],
            }],
            ..Config::default()
        };
        let mut unused = unused_marketplaces(&config);
        unused.sort_unstable();
        assert_eq!(unused, vec!["unused"]);
    }

    // -- orphan_native_permission_keys --

    #[test]
    fn orphan_permissions_none_for_known_engine() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "claude_code".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_permissions_accepts_icm() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "icm".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_permissions_accepts_configured_mcp() {
        let config = Config {
            mcp: vec![McpServer {
                name: "my-server".into(),
                when: vec![],
                transport: McpTransport::Stdio,
                command: Some("echo".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                disabled: false,
                disabled_tools: vec![],
                timeout: None,
            }],
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "my-server".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_permissions_reports_unknown_key() {
        let config = Config {
            capabilities: Capabilities {
                native_permissions: BTreeMap::from([(
                    "mcp__unknown-server".into(),
                    NativePermissionRules::default(),
                )]),
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let orphans = orphan_native_permission_keys(&config);
        assert_eq!(orphans, vec!["mcp__unknown-server"]);
    }

    #[test]
    fn orphan_permissions_empty_no_permissions() {
        let config = Config::default();
        let orphans = orphan_native_permission_keys(&config);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    // -- hooks_with_glob_like_matchers --

    fn hook_with_matcher(event: &str, matcher: &str) -> Hook {
        Hook {
            event: event.into(),
            matcher: Some(matcher.into()),
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::Command,
                command: Some("echo hi".into()),
                tool: None,
            },
            bundle_origin: None,
        }
    }

    #[test]
    fn glob_matchers_flags_file_extension_glob() {
        let config = Config {
            capabilities: Capabilities {
                hooks: vec![hook_with_matcher("PreToolUse", "*.rs")],
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let flagged = hooks_with_glob_like_matchers(&config);
        assert_eq!(flagged, vec!["PreToolUse (matcher: '*.rs')".to_string()]);
    }

    #[test]
    fn glob_matchers_accepts_known_tool_name_alternation() {
        let config = Config {
            capabilities: Capabilities {
                hooks: vec![hook_with_matcher("PreToolUse", "^(Write|Edit|MultiEdit)$")],
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let flagged = hooks_with_glob_like_matchers(&config);
        assert!(flagged.is_empty(), "expected empty: {flagged:?}");
    }

    // -- is_local_addr --

    #[test]
    fn is_local_addr_accepts_localhost() {
        assert!(is_local_addr("localhost"));
    }

    #[test]
    fn is_local_addr_accepts_ipv4_loopback() {
        assert!(is_local_addr("127.0.0.1"));
    }

    #[test]
    fn is_local_addr_accepts_ipv6_loopback() {
        assert!(is_local_addr("::1"));
    }

    #[test]
    fn is_local_addr_rejects_remote_ip() {
        assert!(!is_local_addr("10.0.0.4"));
    }

    #[test]
    fn is_local_addr_rejects_hostname() {
        assert!(!is_local_addr("still.local"));
    }

    #[test]
    fn is_local_addr_accepts_ipv6_unspecified() {
        assert!(is_local_addr("::"));
        assert!(is_local_addr("::0"));
    }

    #[test]
    fn is_local_addr_accepts_broader_loopback() {
        assert!(is_local_addr("127.0.0.2"));
        assert!(is_local_addr("127.255.255.254"));
        assert!(!is_local_addr("128.0.0.1"));
    }

    #[test]
    fn is_local_addr_accepts_unspecified_v4() {
        assert!(is_local_addr("0.0.0.0"));
    }

    // -- run_doctor_tool_availability --

    #[test]
    fn tool_avail_no_crash_default_config() {
        let config = Config::default();
        // Should not panic: checks claude + crush (both may warn), no memory entries
        run_doctor_tool_availability(false, &config);
    }

    #[test]
    fn tool_avail_no_crash_with_memory() {
        let config = Config {
            features: Some(Features {
                memory: vec![Memory {
                    server_host: "local".into(),
                    port: 4343,
                    listen_host: "127.0.0.1".into(),
                    when: vec!["local".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                }],
                ..Features::default()
            }),
            host: BTreeMap::from([(
                "local".into(),
                HostEntry {
                    addr: "127.0.0.1".into(),
                },
            )]),
            ..Config::default()
        };
        run_doctor_tool_availability(false, &config);
    }

    #[test]
    fn tool_avail_no_crash_with_remote_memory() {
        let config = Config {
            features: Some(Features {
                memory: vec![Memory {
                    server_host: "remote".into(),
                    port: 4343,
                    listen_host: "0.0.0.0".into(),
                    when: vec!["remote".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                }],
                ..Features::default()
            }),
            host: BTreeMap::from([(
                "remote".into(),
                HostEntry {
                    addr: "10.0.0.4".into(),
                },
            )]),
            ..Config::default()
        };
        run_doctor_tool_availability(false, &config);
    }

    #[test]
    fn tool_avail_no_crash_claude_disabled() {
        let config = Config {
            disabled_engines: vec!["claude_code".into()],
            ..Config::default()
        };
        run_doctor_tool_availability(false, &config);
    }
}
