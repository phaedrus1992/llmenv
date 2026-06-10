use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::config::{Bundle, Config};
use crate::git;
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

/// Outcome of comparing the content hash the agent booted with against the hash
/// llmenv would render now (see [`stale_status`]).
///
/// #196: drift is detected by *content hash*, not folder name. In version mode
/// the folder name is stable across edits (`1.2`), so only the hash recorded in
/// the booted folder's `.llmenv-manifest.json` reveals an in-place change. This
/// is one code path for both [`crate::config::HashingMode`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleStatus {
    /// Booted hash matches the current one — the session is up to date.
    Fresh,
    /// Config drifted since the agent booted; the user should restart.
    Stale { booted: String, current: String },
    /// No booted hash to compare against (llmenv didn't boot this agent, or the
    /// booted folder predates the manifest dotfile).
    Unknown,
}

impl StaleStatus {
    /// True only when the booted config no longer matches the current one.
    #[must_use]
    pub fn is_drift(&self) -> bool {
        matches!(self, StaleStatus::Stale { .. })
    }
}

/// Compare the content hash the agent booted with against the freshly-computed
/// current hash. `booted` is the `content_hash` read from the booted folder's
/// manifest dotfile; `None` when the agent wasn't booted by llmenv or the
/// booted folder has no manifest.
#[must_use]
pub fn stale_status(booted: Option<&str>, current: &str) -> StaleStatus {
    match booted {
        None => StaleStatus::Unknown,
        Some(b) if b == current => StaleStatus::Fresh,
        Some(b) => StaleStatus::Stale {
            booted: b.to_string(),
            current: current.to_string(),
        },
    }
}

/// True when `s` is a SHA-256 content hash as embedded in a strict-mode folder
/// name: exactly 64 lowercase hex digits. Used by `doctor` to tell a strict
/// folder (`{version}-{hash}`) from a version folder (`1.2`) when recovering the
/// builder version for skew detection (#196).
#[must_use]
fn is_content_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Version string shown by `--version`. Built by `build.rs` as
/// `"<pkg-version> (<short-hash>[-dirty])"`, falling back to bare pkg version
/// when the build had no `.git` directory (e.g. crates.io tarball builds).
const VERSION: &str = env!("LLMENV_VERSION");

/// Filesystem-safe version tag used in cache paths. Built by `build.rs` as
/// `"<pkg-version>-<short-hash>"` or bare `<pkg-version>` when no .git is present.
const VERSION_TAG: &str = env!("LLMENV_VERSION_TAG");

#[derive(Parser)]
#[command(
    name = "llmenv",
    version = VERSION,
    about = "Universal scope-aware environment for AI coding agents",
    arg_required_else_help = true
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
    /// Regenerate the materialized config without exporting shell variables
    Regenerate,
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
    /// Show the current resolved environment and active scopes
    Context,
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
    /// Warn if the booted agent config has drifted from the current config.
    ///
    /// Invoked by the Claude Code SessionStart hook: compares the basename of
    /// `CLAUDE_CONFIG_DIR` (the content hash the agent booted with) against the
    /// folder llmenv would materialize now. On drift it prints a restart hint.
    CheckStale,
    /// Emit source config paths into agent context via SessionStart (#289).
    ///
    /// Invoked by the auto-registered SessionStart hook. Outputs a JSON
    /// `hookSpecificOutput.additionalContext` payload so the agent always knows
    /// where its source config lives and won't edit the managed cache directory.
    ConfigContext,
    /// Warn when the agent tries to write a managed cache path (#289).
    ///
    /// Invoked by the auto-registered PreToolUse hook (matcher: Write|Edit|MultiEdit).
    /// Reads the tool call from stdin, checks whether the target path is inside
    /// the llmenv cache, and prints a redirection hint. Always exits 0 (fail-soft).
    ConfigGuard,
    /// Run an agent lifecycle hook (injects ICM memory context over MCP).
    ///
    /// Invoked by the agent runtime, not by users directly. `event` is an
    /// engine-neutral name: session_start | turn_start | session_end.
    HookRun {
        /// Lifecycle event: session_start, turn_start, or session_end
        event: String,
    },
    /// Manage auth credentials for materialized folders (#172)
    Login {
        /// Apply to the global auth cache (all future materializations) rather
        /// than only the current materialized folder
        #[arg(long)]
        global: bool,
    },
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
        Some(Command::Regenerate) => {
            run_regenerate()?;
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
        Some(Command::Context) => {
            run_context(use_color)?;
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
        Some(Command::CheckStale) => {
            run_check_stale(use_color)?;
        }
        Some(Command::ConfigContext) => {
            run_config_context();
        }
        Some(Command::ConfigGuard) => {
            run_config_guard();
        }
        Some(Command::HookRun { event }) => {
            crate::hook_run::run(&event)?;
        }
        Some(Command::Login { global }) => {
            run_login(global)?;
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

/// Token-efficiency checks emitted as part of `llmenv doctor`.
///
/// Warns on env-var patterns that degrade context efficiency. Prints validated
/// numeric values only; raw env var strings are never echoed to the terminal to
/// prevent terminal escape sequence injection from crafted env values.
fn run_doctor_token_efficiency(config: &Config, use_color: bool, pass: &str, warn: &str) {
    let info = if use_color { "\u{2139}" } else { "i" };
    eprintln!();
    eprintln!("Token-efficiency checks:");

    match std::env::var("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE") {
        Ok(val) => match val.parse::<u32>() {
            Ok(pct) if pct <= 70 => eprintln!("{pass} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct}"),
            Ok(pct) => eprintln!(
                "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct} (recommend ≤70 for PreCompact cleanup)"
            ),
            Err(_) => {
                eprintln!("{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE has invalid (non-numeric) value")
            }
        },
        Err(_) => eprintln!(
            "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE not set (recommend 50 for PreCompact headroom)"
        ),
    }

    match std::env::var("BASH_MAX_OUTPUT_LENGTH").map(|v| v.parse::<u64>()) {
        Ok(Ok(n)) => eprintln!("{pass} BASH_MAX_OUTPUT_LENGTH={n}"),
        Ok(Err(_)) => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH has invalid (non-numeric) value"),
        Err(_) => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH not set (recommend 10000)"),
    }

    match std::env::var("MAX_MCP_OUTPUT_TOKENS").map(|v| v.parse::<u64>()) {
        Ok(Ok(n)) => eprintln!("{pass} MAX_MCP_OUTPUT_TOKENS={n}"),
        Ok(Err(_)) => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS has invalid (non-numeric) value"),
        Err(_) => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS not set (recommend 10000)"),
    }

    match std::env::var("ENABLE_PROMPT_CACHING_1H") {
        Ok(val) if val.eq_ignore_ascii_case("true") || val == "1" => {
            eprintln!("{pass} ENABLE_PROMPT_CACHING_1H=true (1h cache TTL enabled)")
        }
        Ok(_) => eprintln!("{warn} ENABLE_PROMPT_CACHING_1H has unexpected value (recommend true)"),
        Err(_) => {
            eprintln!("{warn} ENABLE_PROMPT_CACHING_1H not set (recommend true for 1h cache reuse)")
        }
    }

    match std::env::var("CLAUDE_CODE_SUBAGENT_MODEL") {
        Ok(_) => eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL is set"),
        Err(_) => {
            eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL not set (default: claude-sonnet-4-6)")
        }
    }

    let has_context_mode = config.mcp.iter().any(|m| m.name.contains("context-mode"));
    if has_context_mode {
        eprintln!("{pass} context-mode MCP server is configured");
    } else {
        eprintln!("{warn} context-mode MCP not configured (load-bearing for token efficiency)");
        eprintln!("{warn}   → Install context-mode plugin and add to mcp: section in config.yaml");
    }
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

    // Report the active cache layout so `doctor` explains the folder shape on
    // disk (#246). loose → shape-addressed; normal → <version_mm>/<shape>;
    // strict → content-addressed folders.
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

    // Check for version skew: warn if running binary differs from the binary
    // that built the cached materializations (#173). Materialized folders live
    // under `<cache_dir>/<adapter>/`, so scan there — not the cache root, whose
    // children are adapter dirs and `marketplaces/`. Strict folders are
    // `{VERSION_TAG}-{hash}`; normal generation dirs are the bare `version_mm`
    // (e.g. `1.2`). Loose mode has no version axis — its folders are bare shape
    // digests with no version meaning, so the skew check is skipped entirely
    // (a shape would be misread as an unknown "version").
    let adapter_cache = cache_dir.join(ClaudeCodeAdapter.name());
    let skew_relevant = !matches!(config.cache.hashing, crate::config::HashingMode::Loose);
    if let (true, Ok(entries)) = (skew_relevant, std::fs::read_dir(&adapter_cache)) {
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
                continue; // orphaned staging dir, not a materialization
            }
            // Strict folder: split the content hash off the right to recover the
            // version prefix (preserving dashes in a semver prerelease). Version
            // folder: the whole name is the version. Distinguish by whether the
            // tail after the last `-` looks like a 64-hex content hash.
            let version = match dir_name.rsplit_once('-') {
                Some((prefix, tail)) if is_content_hash(tail) => prefix.to_string(),
                _ => dir_name.to_string(),
            };
            cached_versions.push(version);
        }
        cached_versions.sort();
        cached_versions.dedup();
        // Skew if no cached folder was built by *this* binary. A normal-mode
        // generation dir (e.g. `1.2`) matches when it equals the current
        // `version_mm`.
        let version_folder = crate::materialize::cache::version_mm();
        let current_built = |v: &String| v == VERSION_TAG || *v == version_folder;
        if !cached_versions.is_empty() {
            let cached_versions_str = cached_versions.join(", ");
            if !cached_versions.iter().any(current_built) {
                eprintln!(
                    "{warn} Version skew detected: running llmenv {} but cache has versions [{}]",
                    VERSION_TAG, cached_versions_str
                );
                eprintln!("{warn}   → Fix: cargo install --path . --force");
            }
        }
    }

    // Check git remote is reachable (if config_dir is a git repo)
    let config_dir = paths::config_dir()?;
    if is_git_repo(&config_dir) {
        match check_git_remote(&config_dir) {
            Ok(remote) => {
                let safe_url = crate::git::sanitize_git_url(&remote);
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
    // Project scopes are now discovered dynamically from .llmenv.yaml
    // and do not appear in static config; orphan checking is N/A.
    // Surface anything the active marker file did wrong:
    //   - unknown fields (typos, stale schema) so the user can clean them up
    //   - enable_bundles names that don't exist (silent no-op footgun)
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
    }
    for b in &config.bundle {
        let has_emitted_tag = b.tags.iter().any(|t| emitted.contains(t));
        let looks_marker = looks_marker_driven(&b.name, b);
        if !has_emitted_tag && !marker_enabled.contains(&b.name) && !looks_marker {
            eprintln!("{warn} orphan bundle {}: no scope emits its tags", b.name);
            orphan_count += 1;
        }
    }
    for m in &config.mcp {
        let has_emitted_tag = m.tags.iter().any(|t| emitted.contains(t));
        let looks_marker = m.tags.iter().any(|t| tag_looks_marker_sourced(t));
        if !has_emitted_tag && !looks_marker {
            eprintln!("{warn} orphan mcp {}: no scope emits its tags", m.name);
            orphan_count += 1;
        }
    }
    if let Some(mem) = config.features.as_ref().and_then(|f| f.memory.as_ref()) {
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

    // Tag orphans: declared but missing emitter or consumer. Tags that look
    // marker-sourced (e.g., `lang-*` tags) are not flagged as orphaned, since
    // they're expected to be available in projects using marker files.
    let mut tag_universe: HashSet<String> = HashSet::new();
    tag_universe.extend(emitted.iter().cloned());
    tag_universe.extend(consumed.iter().cloned());
    tag_universe.extend(active.tags.iter().cloned());
    let mut tag_orphans: Vec<String> = tag_universe
        .into_iter()
        .filter(|t| {
            let emitted_anywhere =
                emitted.contains(t) || active.tags.contains(t) || tag_looks_marker_sourced(t);
            let consumed_anywhere = consumed.contains(t);
            !(emitted_anywhere && consumed_anywhere)
        })
        .collect();
    tag_orphans.sort();
    for t in &tag_orphans {
        let emitted_anywhere =
            emitted.contains(t) || active.tags.contains(t) || tag_looks_marker_sourced(t);
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

    // Lint for ${CLAUDE_PLUGIN_ROOT} in non-plugin hooks (#174)
    // Check hooks defined in config/bundles that will be materialized to top-level settings.json
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

    run_doctor_token_efficiency(&config, use_color, &pass, &warn);

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
    match git::secure_git()
        .args(["rev-parse", "--git-dir"])
        .current_dir(dir)
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

fn check_git_remote(dir: &Path) -> anyhow::Result<String> {
    let output = git::secure_git()
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

fn shell_escape(s: &str) -> String {
    // For values: use single quotes (prevent all expansions) and escape embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Reject any var in `vars` whose name isn't a valid shell identifier.
/// Applied to adapter-returned env vars before they propagate, so the
/// `export NAME=...` contract holds regardless of which emission path runs.
fn reject_invalid_var_names(env: &[(String, String)]) -> anyhow::Result<()> {
    for (name, _) in env {
        validate_var_name(name)?;
    }
    Ok(())
}

fn validate_var_name(name: &str) -> anyhow::Result<()> {
    // Shell variable names must match [A-Za-z_][A-Za-z0-9_]*
    if name.is_empty() {
        anyhow::bail!("Variable name cannot be empty");
    }
    let first = name.as_bytes()[0] as char;
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
        for (key, value) in &bundle.env {
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
    // emit env vars pointing the agent at it. Failure here exits non-zero
    // so callers (shell hooks, CI) can detect it — silently continuing
    // without CLAUDE_CONFIG_DIR violates the export contract. (#281)
    match build_and_materialize(&config, &config_dir, &active, &firing) {
        Ok(Some((ref cache_path, ref extra_vars))) => {
            tracing::debug!("materialized agent config at {}", cache_path.display());
            for (k, v) in extra_vars {
                vars.insert(k.clone(), v.clone());
            }
            // Auth sync: detect in-session login changes and refresh the
            // stable cache. Non-fatal — export must not fail on auth errors.
            let adapter_root =
                expand_tilde(&config.cache.cache_dir)?.join(ClaudeCodeAdapter.name());
            if let Ok(Some(mut manifest)) =
                crate::materialize::manifest::CacheManifest::read(cache_path)
            {
                crate::auth::detect::sync_auth_on_export(cache_path, &adapter_root, &mut manifest);
            }
        }
        Ok(None) => {
            tracing::debug!("no bundle content directories — skipping materialize");
        }
        Err(e) => return Err(e).context("agent materialization failed"),
    }

    // Introspection env: comma-separated, deterministic order. Scopes get
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

    // ICM context chunk: encode active tags/bundles for tag-scoped memory
    // retrieval across projects. Agents can store memory under keyword
    // "llmenv-tag:<tag>" and it will be retrieved in any scope with that tag.
    let bundles_for_icm = firing.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
    let icm_chunk = crate::icm::generate_context_chunk(&active, &bundles_for_icm);
    vars.insert("LLMENV_ICM_CONTEXT".into(), icm_chunk);

    // Store tag/bundle mappings for SessionStart hook retrieval
    if let Err(e) = crate::icm::store_tag_memory(&active, &bundles_for_icm) {
        tracing::debug!("failed to store ICM tag memory (non-fatal): {e}");
    }

    for (key, value) in vars {
        validate_var_name(&key)?;
        println!("export {}={}", key, shell_escape(&value));
    }

    Ok(())
}

fn run_regenerate() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let config_dir = paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    // Collect firing bundles (same logic as run_export)
    let manually_enabled: BTreeSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            b.tags.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    // Materialize the config
    match build_and_materialize(&config, &config_dir, &active, &firing) {
        Ok(Some((cache_path, _))) => {
            eprintln!("✓ Regenerated config at {}", cache_path.display());
            eprintln!(
                "  Tags: {}",
                active.tags.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            eprintln!(
                "  Bundles: {}",
                firing
                    .iter()
                    .map(|b| b.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            eprintln!("\n  Restart your shell session or source the config to load changes.");
        }
        Ok(None) => {
            eprintln!("✓ No bundle content to materialize");
        }
        Err(e) => return Err(e).context("config regeneration failed"),
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
    let Some((manifest, cache_root)) = build_manifest(config, config_dir, active, firing, false)?
    else {
        return Ok(None);
    };

    // The selection *shape* (#246) addresses the folder in loose/normal mode:
    // active tags ∪ directly-enabled bundles. Bundles come from active scopes'
    // `enable_bundles` (the manually-forced selection), kept separate from tags
    // so the two namespaces can't alias into one shape.
    let tags = &active.tags;
    let bundles: BTreeSet<String> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().cloned())
        .collect();
    let shape = crate::materialize::cache::shape(tags, &bundles);

    let adapter = ClaudeCodeAdapter;
    let adapter_root = cache_root.join(adapter.name());
    let rendered = crate::materialize::materialize_with_mode(
        &manifest,
        &adapter_root,
        config.cache.hashing,
        &shape,
    )?;
    let cache_path = rendered.path;

    // Pre-seed user-elected settings keys (#172). Runs only when settings.json
    // is absent so in-place re-renders never overwrite in-session state.
    crate::adapter::claude_code::seed_settings_if_new(&cache_path, &config.init.seeded_settings)?;

    // Run the adapter writer too — materialize copies raw bundle files, but
    // only the adapter writes the agent-native rules file (CLAUDE.md), the
    // MCP config, and settings.json. It returns the paths it owns; we union
    // them with the generic bundle files to form llmenv's complete owned set
    // for this folder (#196). Idempotent per the adapter contract.
    let adapter_owned = adapter.materialize(&manifest, &cache_path)?;

    // Auth inheritance (#172): inject cached credentials after the adapter has
    // finished its own .claude.json writes (mcpServers upsert). Only fires
    // when the stable cache has an entry.
    let auth_status = inject_cached_auth_if_available(&adapter_root, &cache_path);

    let owned = adapter_owned
        .into_iter()
        .chain(manifest.files.keys().cloned());
    let current = crate::materialize::manifest::CacheManifest::new(&rendered.hash, owned)
        .with_selection(tags.clone(), bundles)
        .with_auth_status(auth_status);
    write_cache_manifest(&cache_path, &current, config.cache.hashing)?;

    let mut env_vars = adapter.env_vars(&cache_path)?;

    // Durable state (#175): the state dir is a stable sibling of the hashed
    // config folders (`<adapter_root>/state`), so it survives every hash change.
    // Emit LLMENV_STATE_DIR plus each configured tool's relocation var, and
    // create the dirs so tools find them on first run.
    let state_dir = crate::materialize::state::state_dir(&adapter_root);
    crate::materialize::state::ensure_state_dirs(&config.state, &state_dir)
        .context("creating durable state directories")?;
    env_vars.extend(crate::materialize::state::state_env_vars(
        &config.state,
        &state_dir,
    ));

    // Defense-in-depth (#67): validate var names at the source, not only at the
    // final emission loop. A future emission path that doesn't route through
    // run_export's validate step can't smuggle a name that would break the
    // `export NAME=...` shell contract.
    reject_invalid_var_names(&env_vars)?;
    Ok(Some((cache_path, env_vars)))
}

/// Inject the most-recently-cached auth entry into a materialized folder (#172).
///
/// Non-fatal: if the cache is empty or the inject fails the folder simply has
/// no pre-seeded auth. Errors are traced at debug so `run_export` stays clean.
fn inject_cached_auth_if_available(
    adapter_root: &std::path::Path,
    cache_path: &std::path::Path,
) -> crate::materialize::manifest::AuthStatus {
    use crate::materialize::manifest::{AuthSource, AuthStatus};
    match crate::auth::choose_auth_for_inheritance(adapter_root) {
        Err(e) => {
            tracing::debug!("auth cache lookup failed (non-fatal): {e}");
            AuthStatus::default()
        }
        Ok(None) => AuthStatus::default(),
        Ok(Some(entry)) => match crate::auth::inject_auth_into_claude_json(cache_path, &entry) {
            Ok(()) => {
                eprintln!("[llmenv] auth: {} (inherited)", entry.email);
                AuthStatus {
                    source: AuthSource::Inherited,
                    id: Some(entry.uuid),
                    email: Some(entry.email),
                }
            }
            Err(e) => {
                tracing::debug!("auth inject failed (non-fatal): {e}");
                AuthStatus::default()
            }
        },
    }
}

/// Write the owned-set manifest dotfile `current` for a freshly materialized
/// folder and, in loose/normal mode, reconcile ghost files left by a prior
/// render (#246).
///
/// `current` already carries the union of the adapter's reported paths and the
/// generic bundle files (`manifest.files` keys), plus the plaintext selection.
/// In loose/normal mode the folder is reused across renders, so any file llmenv
/// owned last time but not this time (`previous − current`) is a ghost and is
/// deleted — but only files llmenv recorded as its own; foreign files (Claude
/// runtime state, a plugin's self-registered settings, #175) are never touched.
/// The dotfile is written last so an interrupted render leaves the *previous*
/// manifest intact. Taking the pre-built `current` keeps this within the
/// ≤5-positional-param limit even with the selection set added.
fn write_cache_manifest(
    cache_path: &Path,
    current: &crate::materialize::manifest::CacheManifest,
    mode: crate::config::HashingMode,
) -> anyhow::Result<()> {
    use crate::materialize::manifest::CacheManifest;

    // Strict folders are content-addressed and never reused, so there are no
    // ghost files to reconcile — the dotfile is pure metadata there. Loose and
    // normal both reuse a folder across renders and need reconciliation.
    if !matches!(mode, crate::config::HashingMode::Strict)
        && let Some(previous) = CacheManifest::read(cache_path)?
    {
        for ghost in previous.stale_against(current) {
            // The previous manifest is deserialized raw, bypassing
            // `CacheManifest::new`'s filter — a tampered dotfile could carry a
            // `..`/absolute path that `join` would resolve outside the cache.
            // Re-check here so reconciliation can never delete a foreign file.
            if crate::paths::is_unsafe_join_target(&ghost) {
                tracing::warn!("refusing to delete unsafe owned path from manifest: {ghost}");
                continue;
            }
            let victim = cache_path.join(&ghost);
            if let Err(e) = std::fs::remove_file(&victim) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("removing stale cache file {}: {e}", victim.display());
                }
            } else {
                tracing::debug!("removed stale cache file {}", victim.display());
            }
        }
    }

    current.write(cache_path)
}

/// Build the merged manifest for the firing bundles, resolving MCP servers and
/// plugins exactly as `build_and_materialize` does — but without writing
/// anything. Returns `Ok(None)` when no firing bundle has a content directory.
/// The returned `cache_root` is the expanded cache dir (shared across adapters).
fn build_manifest(
    config: &Config,
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
    refresh_marketplaces: bool,
) -> anyhow::Result<Option<(MergedManifest, PathBuf)>> {
    let refs = build_bundle_refs(config_dir, active, firing);
    if refs.is_empty() {
        return Ok(None);
    }

    let mut manifest: MergedManifest =
        crate::merge::merge(&config.capabilities, &config.native, &refs)?;
    manifest.mcps =
        crate::mcp::resolve::resolve_mcps(config, &active.tags).context("resolving MCP servers")?;
    manifest.mcps.extend(
        crate::mcp::resolve::resolve_bundle_mcps(&manifest.capabilities.mcp, &active.tags)
            .context(
                "resolving bundle MCP servers \
                 (check mcp: entries in active bundle.yaml files)",
            )?,
    );
    // Detect cross-source name collisions (global vs bundle).
    {
        let mut seen = std::collections::HashSet::new();
        for m in &manifest.mcps {
            if !seen.insert(m.name.as_str()) {
                anyhow::bail!(
                    "mcp name '{}' declared in both config.mcp and a bundle mcp: — \
                     rename one to avoid ambiguity",
                    m.name
                );
            }
        }
    }

    let cache_root = expand_tilde(&config.cache.cache_dir)?;

    let resolved = crate::plugins::resolve::resolve_plugins(config, &active.tags)
        .context("resolving plugins")?;
    manifest.plugins = resolved.plugins;
    manifest.marketplaces = sync_marketplaces(
        config,
        &cache_root,
        resolved.marketplaces,
        refresh_marketplaces,
    )?;

    Ok(Some((manifest, cache_root)))
}

/// `llmenv check-stale`: warn when the booted agent config has drifted (#196).
///
/// Reads the `content_hash` from the booted folder's `.llmenv-manifest.json`
/// (`CLAUDE_CONFIG_DIR` is the full folder path) and compares it against the
/// hash llmenv would render now (`hash_manifest`). One code path for both
/// hashing modes: in version mode the folder name is stable across edits, so
/// only the dotfile hash reveals an in-place change. The hash already folds in
/// marketplace `install_location` + `head`, so a plugin path move surfaces here
/// too. On drift, prints a restart hint to stderr (the agent lifecycle hook
/// relays it into the model's context). Resolution mirrors `export` but writes
/// nothing and skips network marketplace refresh (fastest path).
fn run_check_stale(use_color: bool) -> anyhow::Result<()> {
    let booted = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .and_then(|dir| {
            // The booted content hash lives in the folder's manifest dotfile,
            // not its name (a version-mode name is hash-free). Absent/corrupt
            // dotfile → no comparison baseline (Unknown), never an error.
            crate::materialize::manifest::CacheManifest::read(&dir)
                .ok()
                .flatten()
                .map(|m| m.content_hash)
        });

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let config_dir = paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let manually_enabled: BTreeSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            b.tags.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    let current = match build_manifest(&config, &config_dir, &active, &firing, false)? {
        Some((manifest, _)) => crate::materialize::cache::hash_manifest(&manifest)?,
        // No content to materialize: there is no config folder, so a booted
        // CLAUDE_CONFIG_DIR can't have come from llmenv. Treat as not-drifted.
        None => {
            return Ok(());
        }
    };

    match stale_status(booted.as_deref(), &current) {
        StaleStatus::Stale { .. } => {
            let warn = doctor_warning(use_color);
            eprintln!(
                "{warn} llmenv config changed in place; restart your agent to load it. \
                 (Bundles, MCP wiring, or plugin paths changed since this session started.)"
            );
        }
        StaleStatus::Fresh => {}
        // No booted hash to compare against: llmenv didn't boot this agent
        // (CLAUDE_CONFIG_DIR unset, or the folder predates the manifest
        // dotfile). Not drift, so don't nag — but trace it so "the hook ran but
        // said nothing" is distinguishable from a silent no-op on real drift.
        StaleStatus::Unknown => {
            tracing::debug!(
                "check-stale: no booted manifest hash to compare against; \
                 drift detection skipped (current hash would be {current})"
            );
        }
    }

    Ok(())
}

/// `llmenv config-context`: emit source config paths into SessionStart context (#289).
///
/// Always exits 0 (fail-soft). Warns to stderr if paths cannot be resolved so
/// the operator can diagnose the failure; never silently substitutes wrong paths.
fn run_config_context() {
    let config_path = match paths::config_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("llmenv config-context: failed to resolve config path: {e}");
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "additionalContext":
                        "llmenv config-context: could not resolve config path. \
                         Run `llmenv doctor` to diagnose."
                }
            });
            println!("{output}");
            return;
        }
    };
    let bundles_dir = match paths::config_dir() {
        Ok(d) => d.join("bundles"),
        Err(e) => {
            eprintln!("llmenv config-context: failed to resolve config dir: {e}");
            config_path
                .parent()
                .map(|p| p.join("bundles"))
                .unwrap_or_else(|| PathBuf::from(paths::expand_tilde("~/.config/llmenv/bundles")))
        }
    };

    let text = format!(
        "llmenv source config:\n\
         \u{2022} Config: {config}\n\
         \u{2022} Bundles: {bundles}\n\
         \n\
         To update llmenv config, edit the source files above and run `llmenv regenerate`.\n\
         Do NOT edit files under ~/.cache/llmenv/ \u{2014} they are managed and will be overwritten.",
        config = config_path.display(),
        bundles = bundles_dir.display(),
    );

    let output = serde_json::json!({
        "hookSpecificOutput": {
            "additionalContext": text
        }
    });
    println!("{output}");
}

/// `llmenv config-guard`: warn on PreToolUse Write/Edit to managed cache paths (#289).
///
/// Reads the Claude Code hook payload from stdin. If the target path is inside the
/// llmenv cache directory, prints a redirection hint. Always exits 0 (fail-soft).
fn run_config_guard() {
    use std::io::Read;

    let mut stdin_buf = String::new();
    // Fix C: surface stdin read failures via stderr instead of silently discarding
    // them — the guard becomes a no-op on failure, but the operator can see why.
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv config-guard: failed to read stdin: {e}");
        return;
    }

    // Locate the `claude-code` adapter dir in the CLAUDE_CONFIG_DIR path
    // and take its parent as cache_root. This works for all hashing modes:
    //   Normal  → <root>/claude-code/<version>/<shape>  (3 levels below root)
    //   Loose   → <root>/claude-code/<shape>            (2 levels below root)
    //   Strict  → <root>/claude-code/<VERSION>-<hash>   (2 levels below root)
    // Walking up to find "claude-code" and taking its parent is invariant to depth.
    let default_cache = PathBuf::from(paths::expand_tilde("~/.cache/llmenv"));
    let cache_root = match std::env::var("CLAUDE_CONFIG_DIR") {
        Err(_) => default_cache, // expected when not running inside a hook
        Ok(dir) => {
            let path = PathBuf::from(&dir);
            match path
                .ancestors()
                .skip(1)
                .find(|p| p.file_name().map(|n| n == "claude-code").unwrap_or(false))
                .and_then(|p| p.parent().map(PathBuf::from))
            {
                Some(root) => root,
                None => {
                    eprintln!(
                        "llmenv config-guard: could not locate cache root from \
                         CLAUDE_CONFIG_DIR={dir}; falling back to default"
                    );
                    default_cache
                }
            }
        }
    };

    // Non-empty stdin that fails JSON parsing means the hook payload format
    // changed — log it so payload format mismatches are operator-visible.
    let parsed = serde_json::from_str::<serde_json::Value>(&stdin_buf);
    let file_path = match parsed {
        Ok(v) => v
            .get("tool_input")
            .and_then(|ti| ti.get("file_path"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(e) => {
            if !stdin_buf.trim().is_empty() {
                eprintln!("llmenv config-guard: failed to parse hook payload: {e}");
            }
            None
        }
    };

    let Some(path_str) = file_path else {
        return;
    };

    let expanded = paths::expand_tilde(&path_str);
    if is_within_cache(&cache_root, &expanded) {
        let config_path = paths::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| paths::expand_tilde("~/.config/llmenv/config.yaml"));
        println!(
            "\u{26a0} llmenv: {path_str} is inside the managed cache and will be overwritten \
             on the next config regeneration.\n\
             Edit your source config instead: {config_path}\n\
             Then run: llmenv regenerate"
        );
    }
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
        let sync_result = crate::plugins::cache::sync_marketplace(cache_root, market, refresh);
        match sync_result {
            Ok(state) => {
                rm.install_location = Some(state.install_location.to_string_lossy().into_owned());
                rm.head = state.head;
                out.push(rm);
            }
            // (#282) During export (refresh=false), a marketplace that isn't cloned
            // locally should not abort materialization — warn and skip so
            // CLAUDE_CONFIG_DIR can still be emitted. run_plugin_sync (refresh=true)
            // still propagates: an explicit sync that can't reach the remote is a
            // real failure the user needs to see.
            Err(crate::plugins::cache::SyncError::NotCloned { .. }) => {
                eprintln!(
                    "warning: marketplace '{}' not yet cloned\n  → plugins from this marketplace are excluded; run `llmenv plugin-sync` to fetch it",
                    rm.name
                );
            }
            Err(e) => return Err(anyhow::anyhow!("syncing marketplace '{}': {e}", rm.name)),
        }
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
            if crate::paths::is_unsafe_join_target(name) {
                tracing::warn!("rejecting bundle name with traversal/absolute path: {name}");
                return;
            }
            let path = bundles_dir.join(name);
            if !path.exists() {
                tracing::warn!(
                    "bundle '{}' has no content directory at {}; \
                     skipping (vars-only bundle, or missing/deleted directory)",
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

fn emit_hook_guards() {
    // Guard 1: skip in non-interactive shells (no 'i' in $-) — e.g. subshells
    // spawned by Claude Code's Bash tool have no prompt and should never render.
    println!("  [[ $- != *i* ]] && return");
    // Guard 2: skip if environment is already active — avoids redundant re-renders
    // when a child interactive shell inherits the already-active environment.
    println!("  [[ -n \"$LLMENV_STATE_DIR\" ]] && return");
}

fn run_hook(shell: &str) -> anyhow::Result<()> {
    match shell {
        "zsh" => {
            println!("__llmenv_precmd() {{");
            emit_hook_guards();
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
            emit_hook_guards();
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
        Some(p) => {
            let path_str = p
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("init path is not valid UTF-8: {}", p.display()))?;
            expand_tilde(path_str)?
        }
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

    let template = crate::config::template::generate_template();
    std::fs::write(&config_path, template)
        .with_context(|| format!("writing template to {}", config_path.display()))?;
    eprintln!("Created template config at {}", config_path.display());

    let config = Config::load(&config_path).with_context(|| {
        format!(
            "validating newly-written config at {}",
            config_path.display()
        )
    })?;
    eprintln!("✓ Config validated successfully");

    let agents_path = config_dir.join("AGENTS.md");
    if !agents_path.exists() {
        let agents_template = r#"# Agent Orientation

This directory contains llmenv configuration. Agents (Claude Code, Copilot, Gemini CLI)
operating here will have access to the merged config and bundles.

## Key Files & Directories

- **config.yaml** — Main configuration. Declares scopes, bundles, MCP servers, state locations.
- **bundles/** — Bundle directories. Each bundle contains files merged into agent config:
  - `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` — Agent instructions (loaded automatically)
  - `skills/` — Custom skills directory
  - `hooks/` — Hook definitions
  - `mcp.json` — MCP server configurations (optional)
- **state/** — Durable per-tool state (managed by llmenv; don't write here directly)
- **.llmenv.yaml** — Project scope marker (place in project roots, not here)

## Where to Add Things

### New Agent Instructions
Create or edit a bundle's `CLAUDE.md` (Claude Code), `AGENTS.md` (all agents), or `GEMINI.md` (Gemini CLI):
```
bundles/myname/CLAUDE.md
```

### New Skills
Add to a bundle's `skills/` directory:
```
bundles/myname/skills/my-skill.json
```

### New Hooks
Add to a bundle's `hooks/` directory or declare in `config.yaml`:
```
bundles/myname/hooks/some-hook.sh
```

### MCP Servers
Either add to a bundle's `mcp.json` or declare in `config.yaml` under `mcp:`.

### Per-Tool Durable State
Declare in `config.yaml` under `state: tools:`:
```yaml
state:
  tools:
    - env: MY_TOOL_STATE_DIR
      subdir: my-tool
```

## Scopes & Tags

Scopes (network, host, user, project) emit **tags** when they match. Bundles and other
resources fire when one of their tags is in the active tag set. See `config.yaml` comments
for examples.

---

For more, see the llmenv documentation and the `config.yaml` template comments.
"#;

        std::fs::write(&agents_path, agents_template)
            .with_context(|| format!("writing agents template to {}", agents_path.display()))?;
        eprintln!(
            "Created agent orientation guide at {}",
            agents_path.display()
        );
    }

    let readme_path = config_dir.join("README.md");
    let readme_content = r#"# llmenv Configuration

This directory contains your llmenv configuration.

## Layout

- **config.yaml** — Main configuration file. Declares scopes, bundles, MCP servers,
  plugins, and the memory backend. Edit this to define which environments activate
  in which contexts (networks, hosts, users, projects).

- **.llmenv.yaml** — Project markers (one per project). Drop a marker file at the
  root of any project directory to give that project its own scope, tags, and
  enabled bundles. llmenv discovers these by walking upward from the current
  directory.

- **bundles/** — Bundle content directories. Each directory here matches a
  `bundle:` entry in `config.yaml`. Bundles can contain:
  - YAML files (`bundle.yaml`) with MCP servers, hooks, and other capabilities
  - Environment variables
  - Plugin declarations

## Getting Started

1. **Edit config.yaml** to add your first scopes (network, host, user) and a bundle.
   See the comments in the file for examples and reference the [Configuration docs](https://phaedrus1992.github.io/llmenv/docs/configuration).

2. **Install the shell hook** to activate llmenv on every prompt:

   ```bash
   eval "$(llmenv hook zsh)"      # Add to ~/.zshrc
   # or
   eval "$(llmenv hook bash)"     # Add to ~/.bashrc
   ```

3. **Verify setup**:

   ```bash
   llmenv doctor        # Check for configuration errors
   llmenv status        # Show active scopes and tags
   llmenv export        # Preview the exported environment
   ```

4. **Add projects** by creating `.llmenv.yaml` marker files at project roots.

## Concepts

- **Scopes** — Describe where you are (network/host/user/project). Each emits **tags**.
- **Tags** — Labels that trigger bundles, MCP servers, plugins, and memory.
- **Bundles** — Fire when their tags match the active set. Contribute config, env vars, and capabilities.
- **Materialize** — llmenv combines active scopes/bundles into a content-addressed config directory.
- **Adapter** — Renders the materialized config into the agent's native format (e.g. Claude Code).

## Documentation

- [Getting Started](https://phaedrus1992.github.io/llmenv/docs/getting-started) — First-run walkthrough
- [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration) — Complete schema reference
- [Concepts](https://phaedrus1992.github.io/llmenv/docs/concepts) — Scopes, tags, bundles, precedence
- [Main Site](https://phaedrus1992.github.io/llmenv/) — All documentation
"#;
    if !readme_path.exists() {
        std::fs::write(&readme_path, readme_content)
            .with_context(|| format!("writing README to {}", readme_path.display()))?;
        eprintln!("Created README at {}", readme_path.display());
    }

    // Interactive first-run prompts (only when stdin is a TTY).
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        let adapter_root = expand_tilde(&config.cache.cache_dir)?.join(ClaudeCodeAdapter.name());
        run_init_auth_prompt(&adapter_root)?;
        run_init_settings_prompt(&config_dir, &config_path)?;
    }

    Ok(())
}

/// Interactive auth setup for `llmenv init` (#172).
///
/// Prompts the user to (a) login via `claude auth login`, (b) import from the
/// default `~/.claude/.claude.json`, or (c) skip. Only runs when stdin is a TTY.
fn run_init_auth_prompt(adapter_root: &Path) -> anyhow::Result<()> {
    use dialoguer::Select;
    let selection = Select::new()
        .with_prompt("Configure authentication for new Claude Code sessions?")
        .items(&[
            "Login fresh via `claude auth login`",
            "Import from ~/.claude (copy existing login)",
            "Skip (configure later with `llmenv login`)",
        ])
        .default(2)
        .interact_opt()
        .map_err(|e| anyhow::anyhow!("auth prompt failed: {e}"))?;

    match selection {
        Some(0) => {
            // Launch claude auth login in a temp dir, capture the result.
            eprintln!("Launching Claude Code login...");
            run_login_capture(adapter_root, None)?;
        }
        Some(1) => {
            let default_claude =
                PathBuf::from(paths::expand_tilde("~/.claude")).join(".claude.json");
            if default_claude.exists() {
                import_auth_from_file(&default_claude, adapter_root)?;
            } else {
                eprintln!(
                    "~/.claude/.claude.json not found — skipping. \
                     Run `llmenv login` after authenticating."
                );
            }
        }
        _ => {
            eprintln!("Skipping auth setup. Run `llmenv login` when ready.");
        }
    }
    Ok(())
}

/// Interactive settings import for `llmenv init` (#172).
///
/// Reads `~/.claude/settings.json`, presents the non-owned keys as a checkbox
/// list, and stores selected keys in `config.yaml` under `init.seeded_settings`.
fn run_init_settings_prompt(config_dir: &Path, config_path: &Path) -> anyhow::Result<()> {
    use crate::adapter::claude_code::LLMENV_OWNED_SETTINGS_KEYS;
    use dialoguer::MultiSelect;

    let global_settings = PathBuf::from(paths::expand_tilde("~/.claude")).join("settings.json");
    if !global_settings.exists() {
        return Ok(());
    }
    let bytes = match std::fs::read(&global_settings) {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };
    let doc: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(obj) = doc.as_object() else {
        return Ok(());
    };
    let candidates: Vec<(&String, &serde_json::Value)> = obj
        .iter()
        .filter(|(k, _)| !LLMENV_OWNED_SETTINGS_KEYS.contains(&k.as_str()))
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }

    let labels: Vec<String> = candidates
        .iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    let chosen = MultiSelect::new()
        .with_prompt(
            "Select settings from ~/.claude/settings.json to seed into new folders \
             (space to toggle, enter to confirm)",
        )
        .items(&labels)
        .interact_opt()
        .map_err(|e| anyhow::anyhow!("settings prompt failed: {e}"))?;

    let Some(indices) = chosen else {
        return Ok(());
    };
    if indices.is_empty() {
        return Ok(());
    }
    let count = indices.len();

    // Load the current config, merge selected keys, write back.
    let mut config = Config::load(config_path)?;
    for idx in indices {
        let (key, val) = candidates[idx];
        config.init.seeded_settings.insert(key.clone(), val.clone());
    }
    let yaml =
        serde_yaml::to_string(&config).map_err(|e| anyhow::anyhow!("serializing config: {e}"))?;
    std::fs::write(config_path, yaml)
        .with_context(|| format!("writing config {}", config_path.display()))?;
    eprintln!("✓ {count} setting(s) added to init.seeded_settings in config.yaml");
    Ok(())
}

/// `llmenv login [--global]` (#172): capture credentials via `claude auth login`
/// and store them in the stable auth cache.
///
/// Without `--global`: updates only the current materialized folder's auth.
/// With `--global`: updates the stable cache (inherited by all future folders).
fn run_login(global: bool) -> anyhow::Result<()> {
    let config = Config::load(&paths::config_path()?)?;
    let adapter_root = expand_tilde(&config.cache.cache_dir)?.join(ClaudeCodeAdapter.name());

    if global {
        eprintln!("Capturing global auth (will be inherited by all new folders)...");
        run_login_capture(&adapter_root, None)?;
    } else {
        // Determine current materialized folder from CLAUDE_CONFIG_DIR.
        let current_folder = std::env::var("CLAUDE_CONFIG_DIR").ok().map(PathBuf::from);
        run_login_capture(&adapter_root, current_folder.as_deref())?;
    }
    Ok(())
}

/// Launch `claude auth login` in a temp dir, capture the resulting auth, and
/// save it to the stable cache. Optionally also writes to `current_folder`.
fn run_login_capture(adapter_root: &Path, current_folder: Option<&Path>) -> anyhow::Result<()> {
    let tmp = tempfile::TempDir::new()
        .map_err(|e| anyhow::anyhow!("creating temp dir for login: {e}"))?;

    let status = std::process::Command::new("claude")
        .args(["auth", "login"])
        .env("CLAUDE_CONFIG_DIR", tmp.path())
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "running `claude auth login` failed: {e}. \
             Is the Claude Code CLI installed and on PATH?"
            )
        })?;

    if !status.success() {
        anyhow::bail!("`claude auth login` exited with status {status}");
    }

    let entry = crate::auth::read_auth_from_dir(tmp.path())?.ok_or_else(|| {
        anyhow::anyhow!(
            "`claude auth login` completed but no oauthAccount found in the result. \
             Try running `claude auth login` directly."
        )
    })?;

    crate::auth::save_auth_entry(adapter_root, &entry)?;
    eprintln!("[llmenv] login: saved auth for {}", entry.email);

    if let Some(folder) = current_folder {
        if folder.is_dir() {
            crate::auth::inject_auth_into_claude_json(folder, &entry)?;
            // Update manifest auth_status to Explicit.
            if let Ok(Some(mut manifest)) =
                crate::materialize::manifest::CacheManifest::read(folder)
            {
                manifest.auth_status = crate::materialize::manifest::AuthStatus {
                    source: crate::materialize::manifest::AuthSource::Explicit,
                    id: Some(entry.uuid),
                    email: Some(entry.email),
                };
                manifest.write(folder)?;
            }
        }
    }
    Ok(())
}

/// Import auth from an existing `.claude.json` file into the stable cache (#172).
fn import_auth_from_file(source: &Path, adapter_root: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", source.display()))?;
    let entry = crate::auth::extract_auth_entry(&doc).ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no oauthAccount block — not logged in. \
             Try `llmenv login` to authenticate.",
            source.display()
        )
    })?;
    crate::auth::save_auth_entry(adapter_root, &entry)?;
    eprintln!("[llmenv] login: imported auth for {}", entry.email);
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
            let env = crate::scope::matcher::Env::detect();
            let active = crate::scope::evaluate(&config, &env);
            if let Some(proj) = active.scopes.iter().find(|s| s.kind == "project") {
                let label = proj.name.as_deref().unwrap_or(&proj.id);
                if let Some(desc) = &proj.description {
                    eprintln!("    Project: {label} — {desc}");
                } else {
                    eprintln!("    Project: {label}");
                }
            } else {
                eprintln!("    Project: (none)");
            }
            eprintln!("  Bundles: {}", config.bundle.len());
        }
        Err(e) => {
            eprintln!("{} Configuration error: {}", doctor_fail(use_color), e);
            return Err(e);
        }
    }

    Ok(())
}

fn run_context(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let consumed = all_consumed_tags(&config);

    let active_ids: HashSet<(&str, &str)> = active
        .scopes
        .iter()
        .map(|s| (s.kind, s.id.as_str()))
        .collect();

    let mut active_scopes: Vec<String> = Vec::new();
    let mut inactive_scopes: Vec<String> = Vec::new();

    let mut classify = |kind: &str, id: &str, tags: &[String]| {
        let is_active = active_ids.contains(&(kind, id));
        let is_orphan = !tags.iter().any(|t| consumed.contains(t));
        let name = format!("{}:{}", kind, id);
        let annotation = annotate(is_active, is_orphan, use_color);

        if is_active {
            active_scopes.push(format!(
                "{} {}{}",
                active_marker(use_color),
                name,
                annotation
            ));
        } else {
            inactive_scopes.push(format!("  {}{}", name, annotation));
        }
    };

    for s in &config.scope.network {
        classify("network", &s.id, &s.tags);
    }
    for s in &config.scope.host {
        classify("host", &s.id, &s.tags);
    }
    for s in &config.scope.user {
        classify("user", &s.id, &s.tags);
    }
    // Project scopes are discovered dynamically; display them from active scopes if present.
    for scope in &active.scopes {
        if scope.kind == "project" {
            classify("project", &scope.id, &scope.tags);
        }
    }

    if !active_scopes.is_empty() {
        println!("Active");
        for line in active_scopes {
            println!("{}", line);
        }
    }

    if !inactive_scopes.is_empty() {
        println!("Inactive");
        for line in inactive_scopes {
            println!("{}", line);
        }
    }

    // Render merged manifest for the active context using existing bundle resolution
    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| b.tags.iter().any(|tag| active.tags.contains(tag)))
        .collect();
    let bundle_refs = build_bundle_refs(config_dir, &active, &firing);
    let manifest = crate::merge::merge(&config.capabilities, &config.native, &bundle_refs)?;

    println!("\nMerged Manifest");
    println!("Hooks");
    if !manifest.capabilities.hooks.is_empty() {
        for hook in &manifest.capabilities.hooks {
            let source = hook
                .bundle_origin
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("config.yaml");
            println!(
                "  {} {} (from {})",
                hook.event,
                hook.matcher.as_deref().unwrap_or("*"),
                source
            );
        }
    } else {
        println!("  (no hooks selected for active context)");
    }

    Ok(())
}

/// Tags emitted by all configured scopes (regardless of whether they match
/// the current env). A tag is "emitted" if it appears in any scope's static
/// `tags` list. Marker-declared tags are not included here — those are only
/// known when the marker actually matches. Project scope tags are discovered
/// dynamically and not included in this static analysis.
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
        .chain(
            config
                .features
                .as_ref()
                .and_then(|f| f.memory.as_ref())
                .iter()
                .flat_map(|m| m.tags.iter().cloned()),
        )
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

/// True if a tag looks like it's sourced from a marker (e.g., `lang-*` tags).
/// Only tags with the `lang-` prefix are considered marker-sourced, since that
/// prefix is enforced by the marker system itself. Exact-string matches like
/// `"web"` would suppress legitimate orphan warnings for user-defined tags.
fn tag_looks_marker_sourced(tag: &str) -> bool {
    tag.starts_with("lang-")
}

/// True if a bundle looks like it could be activated by a `.llmenv.yaml` marker.
/// Marker-driven bundles typically have names matching language/tool patterns
/// (e.g., `rust-dev`, `python-dev`) and are expected to be enabled by project
/// `.llmenv.yaml` files. When no project context is active, doctor shouldn't
/// flag these as orphaned — they're satisfiable even if not currently active.
fn looks_marker_driven(bundle_name: &str, bundle: &Bundle) -> bool {
    let marker_patterns = [
        "rust", "python", "node", "go", "java", "csharp", "c++", "ruby", "php", "swift", "kotlin",
    ];
    let name_matches = marker_patterns.iter().any(|p| bundle_name.contains(p));
    let tag_matches = bundle.tags.iter().any(|t| tag_looks_marker_sourced(t));
    name_matches || tag_matches
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

/// True if the ICM memory backend is active: configured, selected by tags, and
/// this host is the designated server.
fn is_memory_backend_active(config: &Config, active: &ActiveScopes) -> bool {
    if let Some(mem) = config.features.as_ref().and_then(|f| f.memory.as_ref()) {
        let selected = mem.tags.iter().any(|t| active.tags.contains(t));
        let is_server = active_host_ids(active).contains(&mem.server_host);
        selected && is_server
    } else {
        false
    }
}

/// If the memory backend is selected and designates *this* host as its server,
/// return the bind address (`<listen_host>:<port>`) the `mcp-proxy` should
/// listen on. `None` when this host is a memory client (or memory is
/// unconfigured).
///
/// The host portion comes from `memory.listen_host` (default `"127.0.0.1"`).
/// Set `listen_host: "0.0.0.0"` to accept connections on all interfaces.
///
/// This host is the server when its `server_host` matches a matched host-scope
/// id. Host scopes can match on hostname (auto-detected) but a host can also be
/// placed into the topology manually by emitting the relevant tag from any
/// scope — so a host whose network can't be auto-detected can still be made the
/// server by tagging it explicitly.
fn local_memory_server_bind(config: &Config, active: &ActiveScopes) -> Option<String> {
    let mem = config.features.as_ref().and_then(|f| f.memory.as_ref())?;
    if is_memory_backend_active(config, active) {
        // Warn when binding to all interfaces — the ICM daemon is unauthenticated.
        if let Ok(addr) = mem.listen_host.parse::<std::net::IpAddr>()
            && addr.is_unspecified()
        {
            eprintln!(
                "warning: memory.listen_host is '{}' — the ICM proxy will accept \
                 connections on ALL network interfaces. Set a specific IP to restrict access.",
                mem.listen_host
            );
        }
        Some(format!("{}:{}", mem.listen_host, mem.port))
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
    // Project scopes are discovered dynamically; include active project if present.
    for scope in &active.scopes {
        if scope.kind == "project" {
            push(
                &mut rows,
                "project",
                &scope.id,
                &scope.tags,
                &active_ids,
                &consumed,
            );
        }
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
/// backend is listed too (as `icm`). Bundle-declared MCPs are listed with a
/// `bundle` source tag. Orphans (no scope emits any of their tags) are flagged
/// like bundles.
fn run_mcp_ls(use_color: bool) -> anyhow::Result<()> {
    use crate::mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, resolve_bundle_mcps, resolve_mcps};

    let config_path = paths::config_path()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let config = Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let emitted = all_emitted_tags(&config);

    // Resolved entries (active host only) keyed by name, so we can annotate
    // each selected server with its concrete transport.
    let mut all_resolved: std::collections::HashMap<String, ResolvedKind> =
        resolve_mcps(&config, &active.tags)
            .context("resolving MCP servers for listing")?
            .into_iter()
            .map(|m| (m.name, m.kind))
            .collect();

    // Bundle MCPs: merge the active bundles to get their capabilities.mcp,
    // then resolve bundle entries for the active scope.
    let manually_enabled: std::collections::HashSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            b.tags.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();
    let bundle_refs = build_bundle_refs(config_dir, &active, &firing);
    let bundle_mcp_entries = if !bundle_refs.is_empty() {
        crate::merge::merge(&config.capabilities, &config.native, &bundle_refs)
            .context("merging bundles for mcp-ls")?
            .capabilities
            .mcp
    } else {
        vec![]
    };
    let bundle_resolved = resolve_bundle_mcps(&bundle_mcp_entries, &active.tags)
        .context("resolving bundle MCP servers for listing")?;
    for m in bundle_resolved {
        all_resolved.entry(m.name).or_insert(m.kind);
    }

    let detail_for = |name: &str, fallback: &str| match all_resolved.get(name) {
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

    // Bundle MCPs: always active when tagless, tag-filtered when tagged.
    for m in &bundle_mcp_entries {
        let is_active = m.tags.is_empty() || m.tags.iter().any(|t| active.tags.contains(t));
        // Tagless entries are never orphaned — the bundle itself gates them.
        let is_orphan = !m.tags.is_empty() && !m.tags.iter().any(|t| emitted.contains(t));
        let detail = format!("{} (bundle)", detail_for(&m.name, "stdio server"));
        rows.push((m.name.clone(), is_active, is_orphan, detail));
    }

    if let Some(mem) = config.features.as_ref().and_then(|f| f.memory.as_ref()) {
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
    match crate::sync::commit_and_push(&config_dir, "Update llmenv config")? {
        crate::sync::SyncOutcome::NothingToCommit => {
            eprintln!("No changes to commit (working tree clean)");
        }
        crate::sync::SyncOutcome::Pushed => {
            eprintln!("✓ Synced config to GitHub");
        }
    }
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

    // In normal mode the current generation dir (e.g. "1.2") has no
    // `{VERSION_TAG}-` prefix, so StaleOnly must be told its name or it would
    // sweep the live config dir. Loose mode has no version axis (every shape is
    // current) and strict mode is identified by the prefix test — both pass None.
    let current_version = match config.cache.hashing {
        crate::config::HashingMode::Normal => Some(crate::materialize::cache::version_mm()),
        crate::config::HashingMode::Loose | crate::config::HashingMode::Strict => None,
    };

    // prune runs per adapter subdirectory: the generation/VERSION_TAG folders
    // live under `<cache_dir>/<adapter>/`, not directly under `cache_dir`.
    let report = crate::materialize::cache::prune(
        &cache_dir.join(ClaudeCodeAdapter.name()),
        mode,
        config.cache.hashing,
        current_version.as_deref(),
        dry_run,
    )?;

    let verb = if dry_run { "would remove" } else { "removed" };
    for p in &report.removed {
        eprintln!("  {verb}: {}", p.display());
    }
    for p in &report.failed {
        eprintln!("  failed to remove: {}", p.display());
    }
    eprintln!(
        "prune complete: {} {} entry(ies), kept {}",
        verb,
        report.removed.len(),
        report.kept
    );
    if !report.failed.is_empty() {
        eprintln!("  {} entry(ies) could not be removed", report.failed.len());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::HashingMode;
    use crate::materialize::manifest::{CacheManifest, MANIFEST_FILE};

    #[test]
    fn is_content_hash_matches_only_64_lowercase_hex() {
        assert!(is_content_hash(&"a".repeat(64)));
        assert!(is_content_hash(&"0123456789abcdef".repeat(4)));
        assert!(!is_content_hash(&"a".repeat(63)), "too short");
        assert!(!is_content_hash(&"a".repeat(65)), "too long");
        assert!(!is_content_hash(&"A".repeat(64)), "uppercase rejected");
        assert!(!is_content_hash("1.2"), "version folder is not a hash");
        assert!(!is_content_hash(&format!("{}g", "a".repeat(63))), "non-hex");
    }

    /// Build a [`CacheManifest`] the way `build_and_materialize` does — the
    /// union of adapter-owned + generic bundle paths — directly from path names.
    fn built(content_hash: &str, owned: &[&str]) -> CacheManifest {
        CacheManifest::new(content_hash, owned.iter().map(PathBuf::from))
    }

    #[test]
    fn write_cache_manifest_writes_dotfile_in_all_modes() {
        for mode in [HashingMode::Loose, HashingMode::Normal, HashingMode::Strict] {
            let tmp = tempfile::tempdir().unwrap();
            let cache = tmp.path().join("folder");
            std::fs::create_dir_all(&cache).unwrap();
            let current = built("hash1", &["CLAUDE.md", "a.md"]);
            write_cache_manifest(&cache, &current, mode).unwrap();
            let read = CacheManifest::read(&cache).unwrap().unwrap();
            assert_eq!(read.content_hash, "hash1");
            assert!(read.owned.contains("CLAUDE.md"), "adapter-owned recorded");
            assert!(read.owned.contains("a.md"), "generic file recorded");
            assert!(
                cache.join(MANIFEST_FILE).exists(),
                "dotfile written ({mode:?})"
            );
        }
    }

    #[test]
    fn write_cache_manifest_reuse_modes_remove_ghost_files() {
        // #246: a file owned in render N but not N+1 is deleted on N+1 in every
        // folder-reusing mode (loose + normal), while a foreign (never-owned)
        // file in the same folder survives untouched.
        for mode in [HashingMode::Loose, HashingMode::Normal] {
            let tmp = tempfile::tempdir().unwrap();
            let cache = tmp.path().join("folder");
            std::fs::create_dir_all(&cache).unwrap();

            // Render N: owns ghost.md + keep.md.
            let ghost = cache.join("ghost.md");
            std::fs::write(&ghost, b"old").unwrap();
            write_cache_manifest(&cache, &built("h1", &["ghost.md", "keep.md"]), mode).unwrap();

            // A foreign file Claude/a plugin wrote — never in any owned set.
            let foreign = cache.join("foreign-state.json");
            std::fs::write(&foreign, b"plugin state").unwrap();

            // Render N+1: drops ghost.md from the owned set.
            write_cache_manifest(&cache, &built("h2", &["keep.md"]), mode).unwrap();

            assert!(
                !ghost.exists(),
                "ghost owned file removed on re-render ({mode:?})"
            );
            assert!(
                foreign.exists(),
                "foreign file survives reconciliation ({mode:?})"
            );
            let read = CacheManifest::read(&cache).unwrap().unwrap();
            assert_eq!(read.content_hash, "h2");
            assert!(!read.owned.contains("ghost.md"));
        }
    }

    #[test]
    fn write_cache_manifest_refuses_to_delete_outside_cache() {
        // A tampered dotfile (written as raw JSON, bypassing CacheManifest::new's
        // filter) claims to own a path that escapes the folder. Reconciliation
        // must refuse to delete it rather than join + remove_file outside the
        // cache (#196 path-traversal).
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("1.2");
        std::fs::create_dir_all(&cache).unwrap();

        // A victim file a level above the cache folder — must survive.
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, b"do not delete").unwrap();

        // Hand-write a previous manifest with a traversal owned path. Raw JSON
        // so it slips past the constructor's safety filter the way a tampered
        // or corrupt on-disk dotfile would.
        let tampered = r#"{"content_hash":"old","owned":["../victim.txt"]}"#;
        std::fs::write(cache.join(MANIFEST_FILE), tampered).unwrap();

        // Re-render dropping the traversal path from the owned set.
        write_cache_manifest(&cache, &built("new", &[]), HashingMode::Normal).unwrap();

        assert!(
            victim.exists(),
            "reconciliation must never delete a file outside the cache folder"
        );
    }

    #[test]
    fn write_cache_manifest_strict_mode_never_reconciles() {
        // Strict folders are content-addressed and never reused, so even a file
        // that looks like a prior owned file is left alone (no prior manifest is
        // ever read for deletion in strict mode).
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("v1-hash");
        std::fs::create_dir_all(&cache).unwrap();
        // Seed a prior manifest claiming ownership of a file now absent from the
        // render — in a reuse mode this would be deleted; in strict it isn't.
        let prior = CacheManifest::new("old", vec![PathBuf::from("ghost.md")]);
        prior.write(&cache).unwrap();
        let bystander = cache.join("ghost.md");
        std::fs::write(&bystander, b"x").unwrap();

        write_cache_manifest(&cache, &built("new", &[]), HashingMode::Strict).unwrap();
        assert!(
            bystander.exists(),
            "strict mode performs no ghost reconciliation"
        );
    }

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

    // #281: marketplace sync failure must not silently drop CLAUDE_CONFIG_DIR.

    fn marketplace_config(name: &str, source: &str) -> Config {
        Config {
            marketplace: vec![crate::config::Marketplace {
                name: name.into(),
                source: source.into(),
            }],
            ..Config::default()
        }
    }

    fn resolved_marketplace(name: &str) -> crate::plugins::resolve::ResolvedMarketplace {
        crate::plugins::resolve::ResolvedMarketplace {
            name: name.into(),
            source: String::new(),
            install_location: None,
            head: None,
        }
    }

    #[test]
    fn sync_marketplaces_git_not_cloned_non_fatal_when_not_refreshing() {
        // A git marketplace that isn't cloned locally should be skipped (with a
        // warning) during export (refresh=false) so materialization can continue
        // and CLAUDE_CONFIG_DIR is still emitted. (#282)
        let config = marketplace_config("remote", "https://github.com/example/plugins.git");
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_marketplaces(
            &config,
            tmp.path(),
            vec![resolved_marketplace("remote")],
            false,
        );
        assert!(
            result.is_ok(),
            "git not-cloned during export must be non-fatal"
        );
        assert!(
            result.unwrap().is_empty(),
            "non-cloned marketplace is dropped from output"
        );
    }

    #[test]
    fn sync_marketplaces_path_not_exist_fatal() {
        // A path source that doesn't exist is a configuration error and should
        // fail hard, even during export (refresh=false). (#282)
        let config = marketplace_config("missing", "/nonexistent/path/to/plugins");
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_marketplaces(
            &config,
            tmp.path(),
            vec![resolved_marketplace("missing")],
            false,
        );
        assert!(result.is_err(), "path source not existing must be fatal");
    }

    #[test]
    fn sync_marketplaces_propagates_error_when_refreshing() {
        // An explicit plugin-sync (refresh=true) must still fail hard when a
        // marketplace can't be synced, so the user knows the refresh failed. (#281)
        let config = marketplace_config("missing", "/nonexistent/path/to/plugins");
        let tmp = tempfile::tempdir().unwrap();
        let result = sync_marketplaces(
            &config,
            tmp.path(),
            vec![resolved_marketplace("missing")],
            true,
        );
        assert!(result.is_err(), "refresh=true sync failure must propagate");
    }

    // ===== Tests for local_memory_server_bind (#337) =====

    /// Build a minimal Config with a memory backend whose server_host is "srv".
    /// The caller controls `listen_host` and `port`.
    fn memory_config(listen_host: &str, port: u16) -> Config {
        use crate::config::{Features, HostEntry, Memory};
        use std::collections::BTreeMap;
        let mut host = BTreeMap::new();
        host.insert(
            "srv".to_string(),
            HostEntry {
                addr: "srv.local".to_string(),
            },
        );
        Config {
            features: Some(Features {
                memory: Some(Memory {
                    server_host: "srv".to_string(),
                    port,
                    listen_host: listen_host.to_string(),
                    tags: vec!["mem".to_string()],
                    default_topics: vec![],
                }),
            }),
            host,
            ..Config::default()
        }
    }

    /// Build an ActiveScopes with the host-scope "srv" matched and tag "mem" active.
    fn active_as_server() -> ActiveScopes {
        use crate::scope::ActiveScope;
        ActiveScopes {
            scopes: vec![ActiveScope {
                id: "srv".to_string(),
                kind: "host",
                tags: vec!["mem".to_string()],
                project_root: None,
                enable_bundles: vec![],
                name: None,
                description: None,
                unknown_fields: vec![],
            }],
            tags: {
                let mut t = std::collections::BTreeSet::new();
                t.insert("mem".to_string());
                t
            },
        }
    }

    /// Active scopes with no matched host scope — this host is a client, not
    /// the server.
    fn active_as_client() -> ActiveScopes {
        use crate::scope::ActiveScope;
        ActiveScopes {
            scopes: vec![ActiveScope {
                id: "client".to_string(),
                kind: "host",
                tags: vec!["mem".to_string()],
                project_root: None,
                enable_bundles: vec![],
                name: None,
                description: None,
                unknown_fields: vec![],
            }],
            tags: {
                let mut t = std::collections::BTreeSet::new();
                t.insert("mem".to_string());
                t
            },
        }
    }

    #[test]
    fn local_memory_server_bind_defaults_to_loopback() {
        // Default listen_host must be 127.0.0.1 — backward-compatible loopback.
        let config = memory_config("127.0.0.1", 7878);
        let active = active_as_server();
        let bind = local_memory_server_bind(&config, &active);
        assert_eq!(bind, Some("127.0.0.1:7878".to_string()));
    }

    #[test]
    fn local_memory_server_bind_honours_custom_host() {
        // A configured listen_host is forwarded into the bind address.
        let config = memory_config("0.0.0.0", 9000);
        let active = active_as_server();
        let bind = local_memory_server_bind(&config, &active);
        assert_eq!(bind, Some("0.0.0.0:9000".to_string()));
    }

    #[test]
    fn local_memory_server_bind_returns_none_for_client_host() {
        // When this host is not the designated server, no bind address is returned.
        let config = memory_config("127.0.0.1", 7878);
        let active = active_as_client();
        let bind = local_memory_server_bind(&config, &active);
        assert_eq!(bind, None);
    }

    #[test]
    fn local_memory_server_bind_returns_none_when_memory_unconfigured() {
        let config = Config::default();
        let active = active_as_server();
        let bind = local_memory_server_bind(&config, &active);
        assert_eq!(bind, None);
    }

    #[test]
    fn sync_marketplaces_succeeds_when_marketplace_available() {
        // A marketplace whose path source exists should succeed in both modes.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("my-market");
        std::fs::create_dir(&src).unwrap();
        let config = marketplace_config("local", &src.to_string_lossy());
        let cache = tempfile::tempdir().unwrap();
        for refresh in [false, true] {
            let result = sync_marketplaces(
                &config,
                cache.path(),
                vec![resolved_marketplace("local")],
                refresh,
            );
            assert!(
                result.is_ok(),
                "available marketplace should succeed (refresh={refresh})"
            );
            let out = result.unwrap();
            assert_eq!(out.len(), 1);
            assert!(
                out[0].install_location.is_some(),
                "install_location filled in"
            );
        }
    }
}

/// Lexically normalize a path: resolve `..` and `.` components without touching
/// the filesystem. This prevents `..`-based traversal bypasses when checking
/// path prefixes on paths that may not exist yet.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Pure path-prefix check: does `expanded_path` lie within `cache_root`?
///
/// Normalizes `..` components lexically (no filesystem access) so traversal
/// paths like `/cache/llmenv/../../../etc/passwd` cannot bypass the check.
#[must_use]
fn is_within_cache(cache_root: &std::path::Path, expanded_path: &str) -> bool {
    let path = normalize_path(std::path::Path::new(expanded_path));
    path.starts_with(cache_root)
}

#[cfg(test)]
mod config_guard_tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_is_within_cache_basic() {
        let cache_root = std::path::Path::new("/home/user/.cache/llmenv");
        assert!(is_within_cache(
            cache_root,
            "/home/user/.cache/llmenv/config"
        ));
        assert!(is_within_cache(
            cache_root,
            "/home/user/.cache/llmenv/data/file.txt"
        ));
        assert!(!is_within_cache(
            cache_root,
            "/home/user/.config/llmenv/config.yaml"
        ));
        assert!(!is_within_cache(
            cache_root,
            "/home/user/.cache/llmenv-adjacent/file"
        ));
    }

    #[test]
    fn test_is_within_cache_empty_path() {
        let cache_root = std::path::Path::new("/cache");
        assert!(!is_within_cache(cache_root, ""));
    }

    #[test]
    fn test_is_within_cache_dot_dot_escape() {
        let cache_root = std::path::Path::new("/cache/llmenv");
        // After normalization, /cache/llmenv/../../../etc/passwd → /etc/passwd,
        // which does not start with /cache/llmenv.
        let escaped = "/cache/llmenv/../../../etc/passwd";
        assert!(!is_within_cache(cache_root, escaped));
    }

    // Property-based tests
    proptest! {
        #[test]
        fn prop_within_cache_reflexive(ref cache_root in r"/[a-z/]+") {
            let path = std::path::Path::new(cache_root);
            prop_assert!(is_within_cache(path, cache_root));
        }

        #[test]
        fn prop_within_cache_child_paths(ref cache_root in r"/[a-z/]+", ref suffix in r"[a-z0-9]+") {
            let path = std::path::Path::new(cache_root);
            let child = format!("{}/{}", cache_root, suffix);
            prop_assert!(is_within_cache(path, &child));
        }

        #[test]
        fn prop_sibling_paths_not_within(ref cache_root in r"/cache/[a-z]+") {
            let path = std::path::Path::new(cache_root);
            // Adjacent directory with similar prefix should not match
            if let Some(base) = cache_root.rfind('/') {
                let sibling = format!("{}{}", &cache_root[..=base], "other");
                prop_assert!(!is_within_cache(path, &sibling));
            }
        }

        #[test]
        fn prop_no_panic_on_arbitrary_strings(ref cache_root in r"/[a-z/]*", ref path_str in ".*") {
            let path = std::path::Path::new(cache_root);
            // Should never panic, even on malformed paths
            let _ = is_within_cache(path, path_str);
        }
    }
}
