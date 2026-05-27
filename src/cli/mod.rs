use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::config::{Bundle, Config};
use crate::merge::{BundleRef, MergedManifest};
use crate::paths;
use crate::scope::ActiveScopes;
use anyhow::Context;
use clap::{Parser, Subcommand};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

mod style;

pub use style::{
    ColorMode, active_marker, doctor_fail, doctor_pass, doctor_warning, inactive_annotation,
    orphan_annotation, should_use_color,
};

/// Version string shown by `--version`. Built by `build.rs` as
/// `"<pkg-version> (<short-hash>[-dirty])"`, falling back to bare pkg version
/// when the build had no `.git` directory (e.g. crates.io tarball builds).
const VERSION: &str = env!("LLMENV_VERSION");

#[derive(Parser)]
#[command(
    name = "llmenv",
    version = VERSION,
    about = "Universal scope-aware environment for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Color output: auto (default), always, or never
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
}

/// CLI-facing color flag values, mapped to the internal `ColorMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    fn to_mode(self) -> ColorMode {
        match self {
            ColorChoice::Auto => ColorMode::Auto,
            ColorChoice::Always => ColorMode::Always,
            ColorChoice::Never => ColorMode::Never,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Validate adapter wiring and configuration
    Doctor {
        /// Run cache garbage collection after diagnostics
        #[arg(long)]
        gc: bool,
    },
    /// Export environment variables for a scope
    Export {
        /// Scope ID to export
        #[arg(short, long)]
        scope: Option<String>,
        /// Tag filter (optional)
        #[arg(short, long)]
        tag: Option<String>,
    },
    /// Generate shell hook code
    Hook {
        /// Shell type: zsh or bash
        shell: String,
    },
    /// Initialize llmenv configuration
    Init {
        /// Directory to initialize (defaults to the standard config dir)
        path: Option<std::path::PathBuf>,
        /// Repository to clone config from (optional)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Show current environment status
    Status,
    /// List available scopes
    #[command(alias = "scopes")]
    ScopeLs,
    /// List available tags
    #[command(alias = "tags")]
    TagLs,
    /// List available bundles
    #[command(alias = "bundles")]
    BundleLs,
    /// List selected MCP servers with their resolved role and transport
    #[command(name = "mcp-ls", alias = "mcps")]
    McpLs,
    /// List configured plugin marketplaces, marking those referenced by selected plugins
    #[command(name = "marketplace-ls", alias = "marketplaces")]
    MarketplaceLs,
    /// List configured plugins, marking those selected by the active scope
    #[command(name = "plugin-ls", alias = "plugins")]
    PluginLs,
    /// Sync plugin marketplaces into the cache (clone or fast-forward)
    PluginSync,
    /// Sync config with GitHub (git add, commit, push)
    Sync,
    /// Clean stale cache folders
    Prune {
        /// Remove ALL cache folders unconditionally (next export re-materializes all environments)
        #[arg(long)]
        all: bool,
        /// Remove ONLY current-version cache folders older than this duration (e.g., "14d", "1w")
        #[arg(long)]
        older_than: Option<String>,
        /// Preview deletions without removing (applies to both --all and --older-than)
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Resolve color emission once: combine the --color flag with stdout TTY
    // state. `export` deliberately never consults this — its stdout is eval'd.
    use std::io::IsTerminal;
    let use_color = should_use_color(Some(cli.color.to_mode()), std::io::stdout().is_terminal());

    match cli.command {
        Some(Command::Doctor { gc }) => {
            run_doctor(gc, use_color)?;
        }
        Some(Command::Export { scope, tag }) => {
            run_export(scope, tag)?;
        }
        Some(Command::Hook { shell }) => {
            run_hook(&shell)?;
        }
        Some(Command::Init { path, repo }) => {
            run_init(path, repo)?;
        }
        Some(Command::Status) => {
            run_status(use_color)?;
        }
        Some(Command::ScopeLs) => {
            run_scope_ls(use_color)?;
        }
        Some(Command::TagLs) => {
            run_tag_ls(use_color)?;
        }
        Some(Command::BundleLs) => {
            run_bundle_ls(use_color)?;
        }
        Some(Command::McpLs) => {
            run_mcp_ls(use_color)?;
        }
        Some(Command::MarketplaceLs) => {
            run_marketplace_ls(use_color)?;
        }
        Some(Command::PluginLs) => {
            run_plugin_ls(use_color)?;
        }
        Some(Command::PluginSync) => {
            run_plugin_sync()?;
        }
        Some(Command::Sync) => {
            run_sync()?;
        }
        Some(Command::Prune {
            all,
            older_than,
            dry_run,
        }) => {
            run_prune(all, older_than, dry_run)?;
        }
        None => {
            eprintln!("Usage: llmenv [COMMAND]");
            eprintln!("Run 'llmenv --help' for more information.");
        }
    }

    Ok(())
}

/// Validates adapter wiring: file layout, config parse, no silent breakage.
fn run_doctor(gc: bool, use_color: bool) -> anyhow::Result<()> {
    let pass = doctor_pass(use_color);
    let warn = doctor_warning(use_color);

    eprintln!("Running llmenv doctor...");

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    eprintln!("{pass} Configuration loaded from {}", config_path.display());

    // Check that config parses
    eprintln!("{pass} Config is valid YAML");

    // Check cache directory is writable
    let cache_dir = expand_tilde(&config.cache.cache_dir)?;
    std::fs::create_dir_all(&cache_dir).context("cache directory not writable")?;
    eprintln!(
        "{pass} Cache directory is writable: {}",
        cache_dir.display()
    );

    // Check git remote is reachable (if config_dir is a git repo)
    let config_dir = paths::config_dir()?;
    if is_git_repo(&config_dir) {
        match check_git_remote(&config_dir) {
            Ok(remote) => {
                let safe_url = sanitize_git_url(&remote);
                eprintln!("{pass} Git remote reachable: {}", safe_url);
            }
            Err(e) => eprintln!("{warn} Git remote check failed: {}", e),
        }
    } else {
        eprintln!("{warn} Config directory is not a git repo");
    }

    // Orphan detection: anything declared but unreachable from the
    // scope→tag→bundle wiring is dead config. Use current env so marker-only
    // signals (tags/enable_bundles from an active marker) count as live.
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let emitted = all_emitted_tags(&config);
    let consumed = all_consumed_tags(&config);
    let marker_enabled = marker_enabled_bundle_names(&active);

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
    for s in &config.scope.project {
        if !s.tags.iter().any(|t| consumed.contains(t)) {
            eprintln!(
                "{warn} orphan scope project:{}: no bundle consumes its tags",
                s.id
            );
            orphan_count += 1;
        }
    }
    for b in &config.bundle {
        let has_emitted_tag = b.tags.iter().any(|t| emitted.contains(t));
        if !has_emitted_tag && !marker_enabled.contains(&b.name) {
            eprintln!(
                "{warn} orphan bundle {}: no scope emits its tags and no marker enables it",
                b.name
            );
            orphan_count += 1;
        }
    }
    for m in &config.mcp {
        let has_emitted_tag = m.tags.iter().any(|t| emitted.contains(t));
        if !has_emitted_tag {
            eprintln!("{warn} orphan mcp {}: no scope emits its tags", m.name);
            orphan_count += 1;
        }
    }
    if let Some(mem) = &config.memory {
        let has_emitted_tag = mem.tags.iter().any(|t| emitted.contains(t));
        if !has_emitted_tag {
            eprintln!("{warn} orphan memory: no scope emits its tags");
            orphan_count += 1;
        }
        // The memory client URL is built from the server host's `host:` entry;
        // a missing entry can never resolve — flag it early.
        if !config.host.contains_key(&mem.server_host) {
            eprintln!(
                "{warn} memory: server_host '{}' has no entry in the host: table",
                mem.server_host
            );
            orphan_count += 1;
        }
    }
    // Plugin orphans: a collection no scope can select, and a marketplace no
    // selectable collection references. Mirror the bundle/mcp orphan checks.
    {
        use crate::config::split_plugin_ref;

        // Marketplaces referenced by any collection whose tags are emitted
        // somewhere — anything outside this set can never be pulled in.
        let mut referenceable: HashSet<&str> = HashSet::new();
        for c in &config.plugin_collection {
            let selectable = c.tags.iter().any(|t| emitted.contains(t));
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
            if !referenceable.contains(m.name.as_str()) {
                eprintln!(
                    "{warn} orphan marketplace {}: no selectable plugin references it",
                    m.name
                );
                orphan_count += 1;
            }
        }
    }

    // Tag orphans: declared but missing emitter or consumer.
    let mut tag_universe: HashSet<String> = HashSet::new();
    tag_universe.extend(emitted.iter().cloned());
    tag_universe.extend(consumed.iter().cloned());
    tag_universe.extend(active.tags.iter().cloned());
    let mut tag_orphans: Vec<String> = tag_universe
        .into_iter()
        .filter(|t| {
            let emitted_anywhere = emitted.contains(t) || active.tags.contains(t);
            let consumed_anywhere = consumed.contains(t);
            !(emitted_anywhere && consumed_anywhere)
        })
        .collect();
    tag_orphans.sort();
    for t in &tag_orphans {
        let emitted_anywhere = emitted.contains(t) || active.tags.contains(t);
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

fn expand_tilde(path: &str) -> anyhow::Result<PathBuf> {
    if path.starts_with("~/") || path == "~" {
        let home = std::env::var("HOME").context("HOME env var not set")?;
        let expanded = path.replacen("~", &home, 1);
        Ok(PathBuf::from(expanded))
    } else {
        Ok(PathBuf::from(path))
    }
}

fn is_git_repo(dir: &Path) -> bool {
    match std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(dir)
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

fn check_git_remote(dir: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(dir)
        .output()
        .context("failed to get git remote")?;

    if !output.status.success() {
        anyhow::bail!("no remote configured");
    }

    let remote = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(remote)
}

fn sanitize_git_url(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(proto_end) = url.find("://") {
            if at_pos > proto_end {
                let (proto, rest) = url.split_at(proto_end + 3);
                if let Some(host_start) = rest.find('@') {
                    return format!("{}***@{}", proto, &rest[host_start + 1..]);
                }
            }
        } else {
            return format!("***{}", &url[at_pos..]);
        }
    }
    url.to_string()
}

fn shell_escape(s: &str) -> String {
    // For values: use single quotes (prevent all expansions) and escape embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Reject any var in `vars` whose name isn't a valid shell identifier.
/// Applied to adapter-returned env vars before they propagate, so the
/// `export NAME=...` contract holds regardless of which emission path runs.
fn reject_invalid_var_names(vars: &[(String, String)]) -> anyhow::Result<()> {
    for (name, _) in vars {
        validate_var_name(name)?;
    }
    Ok(())
}

fn validate_var_name(name: &str) -> anyhow::Result<()> {
    // Shell variable names must match [A-Za-z_][A-Za-z0-9_]*
    if name.is_empty() {
        anyhow::bail!("Variable name cannot be empty");
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        anyhow::bail!(
            "Variable name '{}' must start with letter or underscore",
            name
        );
    }
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            anyhow::bail!(
                "Variable name '{}' contains invalid character '{}'",
                name,
                ch
            );
        }
    }
    Ok(())
}

fn run_export(scope: Option<String>, tag: Option<String>) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let config_dir = paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    // When the memory backend designates *this* host as its server, ensure the
    // local `mcp-proxy` is alive before agents try to reach it. Failures here
    // are logged but non-fatal — the export must still emit env vars so the
    // shell hook stays usable.
    if let Some(bind) = local_memory_server_bind(&config, &active) {
        match crate::mcp::proxy::default_pid_path() {
            Ok(pid_path) => {
                if let Err(e) = crate::mcp::proxy::ensure_running(
                    &bind,
                    &pid_path,
                    crate::mcp::proxy::spawn_mcp_proxy,
                ) {
                    eprintln!("warning: failed to ensure mcp-proxy running: {e}");
                }
            }
            Err(e) => {
                eprintln!("warning: cannot locate mcp-proxy pidfile: {e}");
            }
        }
    }

    // Throttled pull: check sync interval and fetch+pull if enough time has elapsed
    let interval_secs = config.cache.sync_interval_minutes * 60;
    let state_dir = paths::state_dir()?;
    if let Err(e) = crate::sync::maybe_pull(
        &config_dir,
        &state_dir,
        std::time::Duration::from_secs(interval_secs),
    ) {
        tracing::debug!("throttled pull failed (non-fatal): {e}");
    }

    // A bundle fires when either:
    //   - one of its tags is in the active tag set (normal tag-based firing), OR
    //   - an active scope manually enables it by name via `enable_bundles`
    //     in a marker file.
    // The optional --tag filter still gates either path.
    let manually_enabled: BTreeSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            if let Some(t) = &tag
                && !b.tags.contains(t)
            {
                return false;
            }
            b.tags.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    // Collect env vars from firing bundles only.
    let mut vars = std::collections::BTreeMap::new();
    for bundle in &firing {
        for (key, value) in &bundle.vars {
            vars.insert(key.clone(), value.clone());
        }
    }

    // TODO: Scope filtering would require evaluating scope match conditions
    // (network gateway/ssid/cidr, host hostname, user user, project path/marker)
    // For now, only tag filtering is implemented
    if scope.is_some() {
        eprintln!("warning: scope filtering not yet implemented, exporting all matching tags");
    }

    // Merge + materialize the agent config directory and let the adapter
    // emit env vars pointing the agent at it. Failures here are logged but
    // non-fatal: env vars from bundles still flow through.
    match build_and_materialize(&config, &config_dir, &active, &firing) {
        Ok(Some((cache_path, extra_vars))) => {
            tracing::debug!("materialized agent config at {}", cache_path.display());
            for (k, v) in extra_vars {
                vars.insert(k, v);
            }
        }
        Ok(None) => {
            tracing::debug!("no bundle content directories — skipping materialize");
        }
        Err(e) => {
            eprintln!("warning: agent materialization failed: {e}");
        }
    }

    // Introspection vars: comma-separated, deterministic order. Scopes get
    // a `<kind>:<id>` prefix so the kind is visible without re-running
    // `llmenv scope ls`. Tags come from a BTreeSet (already sorted); bundles
    // are emitted in declaration order.
    let scopes_csv = active
        .scopes
        .iter()
        .map(|s| format!("{}:{}", s.kind, s.id))
        .collect::<Vec<_>>()
        .join(",");
    let tags_csv = active.tags.iter().cloned().collect::<Vec<_>>().join(",");
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
    let winning_project = active
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

    for (key, value) in vars {
        validate_var_name(&key)?;
        println!("export {}={}", key, shell_escape(&value));
    }

    Ok(())
}

type Materialized = (PathBuf, Vec<(String, String)>);

/// Build BundleRefs for firing bundles in scope-precedence order, merge them
/// into a manifest, materialize through the Claude Code adapter, and return
/// the env vars the adapter wants exported. Returns `Ok(None)` when no
/// firing bundle has a content directory on disk.
fn build_and_materialize(
    config: &Config,
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
) -> anyhow::Result<Option<Materialized>> {
    let refs = build_bundle_refs(config_dir, active, firing);
    if refs.is_empty() {
        return Ok(None);
    }

    let mut manifest: MergedManifest =
        crate::merge::merge(&config.capabilities, &config.native, &refs)?;
    manifest.mcps =
        crate::mcp::resolve::resolve_mcps(config, &active.tags).context("resolving MCP servers")?;

    let cache_root = expand_tilde(&config.cache.cache_dir)?;

    let resolved = crate::plugins::resolve::resolve_plugins(config, &active.tags)
        .context("resolving plugins")?;
    manifest.plugins = resolved.plugins;
    manifest.marketplaces = sync_marketplaces(config, &cache_root, resolved.marketplaces, false)?;

    let adapter = ClaudeCodeAdapter;
    let adapter_root = cache_root.join(adapter.name());
    let cache_path = crate::materialize::materialize(&manifest, &adapter_root)?;

    // Run the adapter writer too — materialize copies raw bundle files, but
    // only the adapter writes the agent-native rules file (CLAUDE.md), the
    // MCP config, and settings.json. Idempotent per the adapter contract.
    adapter.materialize(&manifest, &cache_path)?;

    let env_vars = adapter.env_vars(&cache_path)?;
    // Defense-in-depth (#67): validate adapter-returned var names at the
    // source, not only at the final emission loop. A future emission path that
    // doesn't route through run_export's validate step can't smuggle a name
    // that would break the `export NAME=...` shell contract.
    reject_invalid_var_names(&env_vars)?;
    Ok(Some((cache_path, env_vars)))
}

/// Sync each resolved marketplace into the shared cache and fill in its
/// `install_location` + `head`. `refresh` controls whether git sources are
/// network-refreshed (`plugin sync`) or used as-is (`export`).
fn sync_marketplaces(
    config: &Config,
    cache_root: &Path,
    resolved: Vec<crate::plugins::resolve::ResolvedMarketplace>,
    refresh: bool,
) -> anyhow::Result<Vec<crate::plugins::resolve::ResolvedMarketplace>> {
    let by_name: std::collections::HashMap<&str, &crate::config::Marketplace> = config
        .marketplace
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    let mut out = Vec::with_capacity(resolved.len());
    for mut rm in resolved {
        let Some(market) = by_name.get(rm.name.as_str()) else {
            // resolve_plugins only emits declared marketplaces, so this is
            // unreachable; skip rather than panic if config mutated mid-flight.
            out.push(rm);
            continue;
        };
        let state = crate::plugins::cache::sync_marketplace(cache_root, market, refresh)
            .with_context(|| format!("syncing marketplace '{}'", rm.name))?;
        rm.install_location = Some(state.install_location.to_string_lossy().into_owned());
        rm.head = state.head;
        out.push(rm);
    }
    Ok(out)
}

/// Resolve firing bundles to on-disk `BundleRef`s in scope precedence order
/// (network → host → user → project), then unscoped tags in declaration
/// order. Bundles with no content directory under `<config_dir>/bundles/<name>/`
/// are dropped silently — vars-only bundles are valid.
fn build_bundle_refs(
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
) -> Vec<BundleRef> {
    const PRECEDENCE: &[&str] = &["network", "host", "user", "project"];

    let bundles_dir = config_dir.join("bundles");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut refs: Vec<BundleRef> = Vec::new();

    let push_ref =
        |name: &str, precedence: u8, refs: &mut Vec<BundleRef>, seen: &mut BTreeSet<String>| {
            if seen.contains(name) {
                return;
            }
            let path = bundles_dir.join(name);
            if !path.exists() {
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
            if bundle.tags.iter().any(|t| kind_tags.contains(t.as_str())) {
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

fn run_hook(shell: &str) -> anyhow::Result<()> {
    match shell {
        "zsh" => {
            println!("__llmenv_precmd() {{");
            println!("  source <(llmenv export)");
            println!("}}");
            println!();
            println!("# Add to precmd_functions if not already present");
            println!("if [[ ! \" ${{precmd_functions[@]}} \" =~ \" __llmenv_precmd \" ]]; then");
            println!("  precmd_functions+=(\"__llmenv_precmd\")");
            println!("fi");
        }
        "bash" => {
            println!("__llmenv_prompt() {{");
            println!("  source <(llmenv export)");
            println!("}}");
            println!();
            println!("# Prepend to PROMPT_COMMAND if not already present");
            println!("if [[ \"$PROMPT_COMMAND\" != *\"__llmenv_prompt\"* ]]; then");
            println!("  PROMPT_COMMAND=\"__llmenv_prompt;$PROMPT_COMMAND\"");
            println!("fi");
        }
        _ => {
            anyhow::bail!("Unsupported shell: {}. Supported: zsh, bash", shell);
        }
    }

    Ok(())
}

fn run_init(path: Option<std::path::PathBuf>, repo: Option<String>) -> anyhow::Result<()> {
    let config_dir = match path {
        Some(p) => expand_tilde(p.to_string_lossy().as_ref())?,
        None => paths::config_dir()?,
    };
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating config dir {}", config_dir.display()))?;

    if let Some(_repo_url) = repo {
        anyhow::bail!("Git clone not yet implemented");
    }

    let config_path = config_dir.join("config.yaml");
    if config_path.exists() {
        eprintln!("Config already exists at {}", config_path.display());
        return Ok(());
    }

    let template = r#"cache:
  cache_dir: "~/.cache/llmenv"
  sync_interval_minutes: 60

# Scopes are lists — uncomment and fill in as needed.
# scope:
#   network:
#     - id: home
#       match: { ssid: "MyHomeWiFi" }
#       tags: [home]
#   host:
#     - id: laptop
#       match: { hostname: "my-laptop" }
#       tags: [laptop]
#   user:
#     - id: me
#       match: { user: "alice" }
#       tags: [me]
#   project:
#     - id: myapp
#       match: { marker: ".llmenvrc" }
#       tags: [myapp]

# Bundles fire when one of their tags is emitted by a matching scope.
bundle:
  - name: base
    tags: [me]
    vars:
      AGENT: "claude"

# MCP servers are selected by tag, like bundles, and rendered into the agent's
# MCP config (mcp.json for Claude Code). Each is stdio (a command) or remote (a
# url).
# mcp:
#   - name: playwright
#     tags: [me]
#     command: npx
#     args: ["-y", "@playwright/mcp@latest"]

# llmenv's memory backend: one host runs it, every host connects over the
# network. `host:` maps the server host name to a reachable address.
# host:
#   my-laptop:
#     addr: "my-laptop.local"
# memory:
#   server_host: my-laptop
#   port: 7878
#   tags: [me]
"#;
    std::fs::write(&config_path, template)
        .with_context(|| format!("writing template to {}", config_path.display()))?;
    eprintln!("Created template config at {}", config_path.display());

    Config::load(&config_path)?;
    eprintln!("✓ Config validated successfully");

    Ok(())
}

fn run_status(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    match Config::load(&config_path) {
        Ok(config) => {
            eprintln!(
                "{} Configuration loaded from {}",
                doctor_pass(use_color),
                config_path.display()
            );
            eprintln!("  Scopes:");
            eprintln!("    Network: {}", config.scope.network.len());
            eprintln!("    Host: {}", config.scope.host.len());
            eprintln!("    User: {}", config.scope.user.len());
            eprintln!("    Project: {}", config.scope.project.len());
            eprintln!("  Bundles: {}", config.bundle.len());
        }
        Err(e) => {
            eprintln!("{} Configuration error: {}", doctor_fail(use_color), e);
            return Err(e);
        }
    }

    Ok(())
}

/// Tags emitted by all configured scopes (regardless of whether they match
/// the current env). A tag is "emitted" if it appears in any scope's static
/// `tags` list. Marker-declared tags are not included here — those are only
/// known when the marker actually matches.
fn all_emitted_tags(config: &Config) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in &config.scope.network {
        out.extend(s.tags.iter().cloned());
    }
    for s in &config.scope.host {
        out.extend(s.tags.iter().cloned());
    }
    for s in &config.scope.user {
        out.extend(s.tags.iter().cloned());
    }
    for s in &config.scope.project {
        out.extend(s.tags.iter().cloned());
    }
    out
}

/// Tags consumed by any configured bundle, MCP server, or the memory backend.
/// A scope whose tags are consumed by any of these is reachable, not an orphan.
fn all_consumed_tags(config: &Config) -> HashSet<String> {
    config
        .bundle
        .iter()
        .flat_map(|b| b.tags.iter().cloned())
        .chain(config.mcp.iter().flat_map(|m| m.tags.iter().cloned()))
        .chain(config.memory.iter().flat_map(|m| m.tags.iter().cloned()))
        .collect()
}

/// Bundle names referenced via marker `enable_bundles` in the currently
/// active scopes.
fn marker_enabled_bundle_names(active: &ActiveScopes) -> HashSet<String> {
    active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().cloned())
        .collect()
}

/// Ids of host-scopes that matched this host. Used to decide whether the
/// memory backend runs locally: it does when its `server_host` is among these.
fn active_host_ids(active: &ActiveScopes) -> BTreeSet<String> {
    active
        .scopes
        .iter()
        .filter(|s| s.kind == "host")
        .map(|s| s.id.clone())
        .collect()
}

/// If the memory backend is selected and designates *this* host as its server,
/// return the bind address (`0.0.0.0:<port>`) the `mcp-proxy` should listen on.
/// `None` when this host is a memory client (or memory is unconfigured).
///
/// This host is the server when its `server_host` matches a matched host-scope
/// id. Host scopes can match on hostname (auto-detected) but a host can also be
/// placed into the topology manually by emitting the relevant tag from any
/// scope — so a host whose network can't be auto-detected can still be made the
/// server by tagging it explicitly.
fn local_memory_server_bind(config: &Config, active: &ActiveScopes) -> Option<String> {
    let mem = config.memory.as_ref()?;
    let selected = mem.tags.iter().any(|t| active.tags.contains(t));
    let is_server = active_host_ids(active).contains(&mem.server_host);
    if selected && is_server {
        Some(format!("0.0.0.0:{}", mem.port))
    } else {
        None
    }
}

/// Annotation suffix for a listing row, colored when `use_color` is set.
fn annotate(active: bool, orphan: bool, use_color: bool) -> String {
    if active {
        String::new()
    } else if orphan {
        format!(" {}", orphan_annotation(use_color))
    } else {
        format!(" {}", inactive_annotation(use_color))
    }
}

fn run_scope_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let consumed = all_consumed_tags(&config);

    let active_ids: HashSet<(&str, &str)> = active
        .scopes
        .iter()
        .map(|s| (s.kind, s.id.as_str()))
        .collect();

    let mut rows: Vec<(String, bool, bool)> = Vec::new();
    let push = |rows: &mut Vec<(String, bool, bool)>,
                kind: &str,
                id: &str,
                tags: &[String],
                active_ids: &HashSet<(&str, &str)>,
                consumed: &HashSet<String>| {
        let is_active = active_ids.contains(&(kind, id));
        let is_orphan = !tags.iter().any(|t| consumed.contains(t));
        rows.push((format!("{}:{}", kind, id), is_active, is_orphan));
    };
    for s in &config.scope.network {
        push(&mut rows, "network", &s.id, &s.tags, &active_ids, &consumed);
    }
    for s in &config.scope.host {
        push(&mut rows, "host", &s.id, &s.tags, &active_ids, &consumed);
    }
    for s in &config.scope.user {
        push(&mut rows, "user", &s.id, &s.tags, &active_ids, &consumed);
    }
    for s in &config.scope.project {
        push(&mut rows, "project", &s.id, &s.tags, &active_ids, &consumed);
    }

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, is_active, is_orphan) in rows {
        let mark = if is_active {
            active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {}{}",
            mark,
            name,
            annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

fn run_tag_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let emitted = all_emitted_tags(&config);
    let consumed = all_consumed_tags(&config);

    // Universe: every tag referenced anywhere (scopes static, bundles, and
    // marker-supplied tags currently in `active.tags`).
    let mut universe: HashSet<String> = HashSet::new();
    universe.extend(emitted.iter().cloned());
    universe.extend(consumed.iter().cloned());
    universe.extend(active.tags.iter().cloned());

    let mut tags: Vec<String> = universe.into_iter().collect();
    tags.sort();
    for tag in tags {
        let is_active = active.tags.contains(&tag);
        // Orphan if no scope emits it OR no bundle consumes it. (Marker-only
        // tags are emitted by virtue of being in `active.tags` even when not
        // in `emitted`.)
        let emitted_anywhere = emitted.contains(&tag) || active.tags.contains(&tag);
        let consumed_anywhere = consumed.contains(&tag);
        let is_orphan = !(emitted_anywhere && consumed_anywhere);
        let mark = if is_active {
            active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {}{}",
            mark,
            tag,
            annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

fn run_bundle_ls(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let emitted = all_emitted_tags(&config);
    let marker_enabled = marker_enabled_bundle_names(&active);

    // A bundle "fires" iff one of its tags intersects active.tags OR its name
    // appears in any active marker's enable_bundles list. Mirrors the filter
    // used by run_export.
    let firing_names: HashSet<&str> = config
        .bundle
        .iter()
        .filter(|b| {
            b.tags.iter().any(|t| active.tags.contains(t))
                || marker_enabled.contains(b.name.as_str())
        })
        .map(|b| b.name.as_str())
        .collect();

    let mut rows: Vec<(String, bool, bool)> = config
        .bundle
        .iter()
        .map(|b| {
            let is_active = firing_names.contains(b.name.as_str());
            // Orphan: no scope emits any of its tags AND it isn't marker-enabled
            // by any currently active marker. Bundles with no tags at all are
            // also orphans unless marker-enabled.
            let has_emitted_tag = b.tags.iter().any(|t| emitted.contains(t));
            let is_orphan = !has_emitted_tag && !marker_enabled.contains(&b.name);
            (b.name.clone(), is_active, is_orphan)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, is_active, is_orphan) in rows {
        let mark = if is_active {
            active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {}{}",
            mark,
            name,
            annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

/// List configured MCP servers, marking those selected by the active scope and
/// annotating each with its resolved transport for this host. The memory
/// backend is listed too (as `icm`). Orphans (no scope emits any of their tags)
/// are flagged like bundles.
fn run_mcp_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, resolve_mcps};

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let emitted = all_emitted_tags(&config);

    // Resolved entries (active host only) keyed by name, so we can annotate
    // each selected server with its concrete transport.
    let resolved =
        resolve_mcps(&config, &active.tags).context("resolving MCP servers for listing")?;
    let resolved_by_name: std::collections::HashMap<&str, &ResolvedKind> = resolved
        .iter()
        .map(|m| (m.name.as_str(), &m.kind))
        .collect();

    let detail_for = |name: &str, fallback: &str| match resolved_by_name.get(name) {
        Some(ResolvedKind::Stdio { .. }) => "stdio server".to_string(),
        Some(ResolvedKind::Remote { transport, .. }) => {
            format!("{} client", format!("{transport:?}").to_lowercase())
        }
        None => fallback.to_string(),
    };

    let mut rows: Vec<(String, bool, bool, String)> = config
        .mcp
        .iter()
        .map(|m| {
            let is_active = m.tags.iter().any(|t| active.tags.contains(t));
            let is_orphan = !m.tags.iter().any(|t| emitted.contains(t));
            let detail = detail_for(&m.name, &format!("{:?}", m.transport).to_lowercase());
            (m.name.clone(), is_active, is_orphan, detail)
        })
        .collect();
    if let Some(mem) = &config.memory {
        let is_active = mem.tags.iter().any(|t| active.tags.contains(t));
        let is_orphan = !mem.tags.iter().any(|t| emitted.contains(t));
        let detail = detail_for(MEMORY_MCP_NAME, "memory");
        rows.push((MEMORY_MCP_NAME.to_string(), is_active, is_orphan, detail));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, is_active, is_orphan, detail) in rows {
        let mark = if is_active {
            active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {} ({}){}",
            mark,
            name,
            detail,
            annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

/// List configured marketplaces, marking those referenced by a plugin the
/// active scope selects. A marketplace is an orphan when no plugin in any
/// scope-emittable collection references it (nothing can ever pull it in).
fn run_marketplace_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::config::split_plugin_ref;

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let emitted = all_emitted_tags(&config);

    // Marketplaces referenced by plugins in collections the active scope selects.
    let active_refs: std::collections::HashSet<&str> = config
        .plugin_collection
        .iter()
        .filter(|c| c.tags.iter().any(|t| active.tags.contains(t)))
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m))
        .collect();
    // Marketplaces referenced by any collection that *could* be selected by some
    // scope (its tags are emitted somewhere). The complement is orphan.
    let referenceable: std::collections::HashSet<&str> = config
        .plugin_collection
        .iter()
        .filter(|c| c.tags.iter().any(|t| emitted.contains(t)))
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m))
        .collect();

    let mut rows: Vec<(String, bool, bool, String)> = config
        .marketplace
        .iter()
        .map(|m| {
            let is_active = active_refs.contains(m.name.as_str());
            let is_orphan = !referenceable.contains(m.name.as_str());
            let kind = match m.classify_source() {
                crate::config::MarketplaceSource::Git => "git",
                crate::config::MarketplaceSource::Path => "path",
            };
            (m.name.clone(), is_active, is_orphan, kind.to_string())
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, is_active, is_orphan, kind) in rows {
        let mark = if is_active {
            active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {} ({}){}",
            mark,
            name,
            kind,
            annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

/// List configured plugins (flattened across collections), marking those the
/// active scope selects. A plugin is an orphan when no scope emits any of its
/// collection's tags.
fn run_plugin_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::config::split_plugin_ref;

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let emitted = all_emitted_tags(&config);

    // One row per (collection, plugin) pair so provenance is visible — the same
    // plugin in two collections shows twice, mirroring the config as authored.
    let mut rows: Vec<(String, bool, bool, String)> = Vec::new();
    for collection in &config.plugin_collection {
        let is_active = collection.tags.iter().any(|t| active.tags.contains(t));
        let is_orphan = !collection.tags.iter().any(|t| emitted.contains(t));
        for plugin in &collection.plugins {
            let display = split_plugin_ref(plugin)
                .map_or_else(|| plugin.clone(), |(m, p)| format!("{p}@{m}"));
            rows.push((display, is_active, is_orphan, collection.name.clone()));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, is_active, is_orphan, collection) in rows {
        let mark = if is_active {
            active_marker(use_color)
        } else {
            " ".to_string()
        };
        println!(
            "{} {} (from {}){}",
            mark,
            name,
            collection,
            annotate(is_active, is_orphan, use_color)
        );
    }
    Ok(())
}

/// Sync every configured marketplace into the shared cache: git sources are
/// cloned on first use and fast-forwarded on subsequent runs; path sources are
/// resolved in place. Reports each marketplace's resolved location + HEAD.
fn run_plugin_sync() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let cache_root = expand_tilde(&config.cache.cache_dir)?;

    if config.marketplace.is_empty() {
        eprintln!("No marketplaces configured.");
        return Ok(());
    }

    for m in &config.marketplace {
        let state = crate::plugins::cache::sync_marketplace(&cache_root, m, true)
            .with_context(|| format!("syncing marketplace '{}'", m.name))?;
        let head = state.head.as_deref().unwrap_or("(local path)");
        println!(
            "✓ {} → {} [{}]",
            m.name,
            state.install_location.display(),
            head
        );
    }
    Ok(())
}

fn run_sync() -> anyhow::Result<()> {
    let config_dir = paths::config_dir()?;

    // Stage all changes in config_dir
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(&config_dir)
        .status()
        .context("failed to stage changes (git add -A)")?;

    // Commit (allow empty if nothing changed)
    let commit_result = std::process::Command::new("git")
        .args(["commit", "-m", "Update llmenv config"])
        .current_dir(&config_dir)
        .status()
        .context("failed to create commit (git commit)")?;

    if !commit_result.success() {
        eprintln!("No changes to commit (working tree clean)");
        return Ok(());
    }

    // Push to origin
    std::process::Command::new("git")
        .args(["push"])
        .current_dir(&config_dir)
        .status()
        .context("failed to push config (git push)")?;

    eprintln!("✓ Synced config to GitHub");
    Ok(())
}

fn run_prune(all: bool, older_than: Option<String>, dry_run: bool) -> anyhow::Result<()> {
    use crate::materialize::cache::PruneMode;

    // Validate flag combinations
    if all && older_than.is_some() {
        anyhow::bail!("--all and --older-than are mutually exclusive");
    }

    let mode = if all {
        PruneMode::All
    } else if let Some(duration_str) = &older_than {
        let dur = humantime::parse_duration(duration_str)
            .with_context(|| format!("failed to parse --older-than duration: {}", duration_str))?;
        PruneMode::OlderThan(dur)
    } else {
        // Default: clean stale (version-mismatched) folders + orphaned *.tmp.
        PruneMode::StaleOnly
    };

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let cache_dir = expand_tilde(&config.cache.cache_dir)?;

    let report = crate::materialize::cache::prune(&cache_dir, mode, dry_run)?;

    let verb = if dry_run { "would remove" } else { "removed" };
    for p in &report.removed {
        eprintln!("  {verb}: {}", p.display());
    }
    eprintln!(
        "prune complete: {} {} entry(ies), kept {}",
        verb,
        report.removed.len(),
        report.kept
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_protects_metacharacters() {
        assert_eq!(shell_escape("normal"), "'normal'");
        assert_eq!(shell_escape("with'quote"), "'with'\\''quote'");
        assert_eq!(shell_escape("$var"), "'$var'");
        assert_eq!(shell_escape("$(cmd)"), "'$(cmd)'");
        assert_eq!(shell_escape("`cmd`"), "'`cmd`'");
    }

    #[test]
    fn validate_var_name_accepts_valid_names() {
        assert!(validate_var_name("MY_VAR").is_ok());
        assert!(validate_var_name("_private").is_ok());
        assert!(validate_var_name("var123").is_ok());
        assert!(validate_var_name("x").is_ok());
    }

    #[test]
    fn validate_var_name_rejects_invalid_names() {
        assert!(validate_var_name("").is_err());
        assert!(validate_var_name("123var").is_err());
        assert!(validate_var_name("my-var").is_err());
        assert!(validate_var_name("my var").is_err());
        assert!(validate_var_name("my$var").is_err());
    }

    #[test]
    fn reject_invalid_var_names_passes_valid_and_fails_invalid() {
        let ok = vec![
            ("CLAUDE_CONFIG_DIR".to_string(), "/x".to_string()),
            ("_PRIVATE".to_string(), "y".to_string()),
        ];
        assert!(reject_invalid_var_names(&ok).is_ok());

        let bad = vec![("bad-name".to_string(), "v".to_string())];
        assert!(reject_invalid_var_names(&bad).is_err());

        let bad_leading_digit = vec![("1ABC".to_string(), "v".to_string())];
        assert!(reject_invalid_var_names(&bad_leading_digit).is_err());
    }

    #[test]
    fn hook_zsh_generates_precmd_code() {
        let result = run_hook("zsh");
        assert!(result.is_ok());
    }

    #[test]
    fn hook_bash_generates_prompt_command_code() {
        let result = run_hook("bash");
        assert!(result.is_ok());
    }

    #[test]
    fn hook_unsupported_shell_fails() {
        let result = run_hook("fish");
        assert!(result.is_err());
    }

    #[test]
    fn expand_tilde_home() {
        let home = std::env::var("HOME")
            .context("HOME env var not set")
            .unwrap();
        let result = expand_tilde("~/test").unwrap();
        assert_eq!(result, PathBuf::from(format!("{}/test", home)));
    }

    #[test]
    fn expand_tilde_tilde_only() {
        let home = std::env::var("HOME")
            .context("HOME env var not set")
            .unwrap();
        let result = expand_tilde("~").unwrap();
        assert_eq!(result, PathBuf::from(&home));
    }

    #[test]
    fn expand_tilde_no_tilde() {
        let result = expand_tilde("/absolute/path").unwrap();
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn sanitize_git_url_http_with_credentials() {
        let url = "https://user:password@github.com/owner/repo.git";
        let sanitized = sanitize_git_url(url);
        assert_eq!(sanitized, "https://***@github.com/owner/repo.git");
    }

    #[test]
    fn sanitize_git_url_ssh() {
        let url = "git@github.com:owner/repo.git";
        let sanitized = sanitize_git_url(url);
        assert_eq!(sanitized, "***@github.com:owner/repo.git");
    }

    #[test]
    fn sanitize_git_url_no_credentials() {
        let url = "https://github.com/owner/repo.git";
        let sanitized = sanitize_git_url(url);
        assert_eq!(sanitized, url);
    }
}
