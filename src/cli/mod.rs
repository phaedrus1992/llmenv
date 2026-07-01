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

mod doctor;
mod status;
mod style;

pub use style::{
    ColorMode, active_marker, doctor_fail, doctor_info, doctor_pass, doctor_warning,
    inactive_annotation, orphan_annotation, should_use_color,
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
        /// Check all scopes and bundles for orphans, not just the active context
        #[arg(long)]
        all: bool,
    },
    /// Export environment variables for a scope
    Export {
        /// Scope ID to export
        #[arg(short, long)]
        scope: Option<String>,
        /// Tag filter (optional)
        #[arg(short, long)]
        tag: Option<String>,
        /// Annotate each variable with its source bundle/scope
        #[arg(long)]
        explain: bool,
        /// Compress the materialized AGENTS.md (CLAUDE.md) to reduce token cost
        #[arg(long)]
        compress: bool,
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
    Status {
        /// Show only a specific section (scopes, tags, bundles, mcps, plugins, marketplaces)
        #[arg(value_enum)]
        section: Option<status::StatusSection>,
    },
    /// Show the current resolved environment and active scopes
    Context {
        /// Filter to a specific bundle name
        #[arg(long)]
        bundle: Option<String>,
        /// Show activation reasons for each scope and bundle
        #[arg(long)]
        why: bool,
    },
    /// List available scopes (deprecated: use 'status scopes')
    #[command(alias = "scopes", hide = true)]
    ScopeLs,
    /// List available tags (deprecated: use 'status tags')
    #[command(alias = "tags", hide = true)]
    TagLs,
    /// List available bundles (deprecated: use 'status bundles')
    #[command(alias = "bundles", hide = true)]
    BundleLs,
    /// List selected MCP servers (deprecated: use 'status mcps')
    #[command(name = "mcp-ls", alias = "mcps", hide = true)]
    McpLs,
    /// List configured plugin marketplaces (deprecated: use 'status marketplaces')
    #[command(name = "marketplace-ls", alias = "marketplaces", hide = true)]
    MarketplaceLs,
    /// List configured plugins (deprecated: use 'status plugins')
    #[command(name = "plugin-ls", alias = "plugins", hide = true)]
    PluginLs,
    /// Sync plugin marketplaces into the cache (clone or fast-forward)
    PluginSync,
    /// Sync config with GitHub (git add, commit, push)
    Sync {
        /// Preview what would be committed/pushed without doing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Warn if the booted agent config has drifted from the current config.
    ///
    /// Invoked by the Claude Code SessionStart hook: compares the basename of
    /// `CLAUDE_CONFIG_DIR` (the content hash the agent booted with) against the
    /// folder llmenv would materialize now. On drift it prints a restart hint.
    CheckStale {
        /// Re-export automatically when drift is detected
        #[arg(long)]
        auto_fix: bool,
        /// Engine that invoked this hook (e.g. "claude_code"). Accepted for
        /// forward-compatibility; the value is stored for future dispatch use.
        #[arg(long, default_value = "claude_code")]
        engine: String,
    },
    /// Validate configuration syntax and wiring without running diagnostics
    Validate,
    /// Open configuration or a named bundle file in $EDITOR
    Edit {
        /// Bundle name to edit (defaults to main config.yaml)
        bundle: Option<String>,
    },
    /// Generate shell completion scripts
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
    /// Emit source config paths into agent context via SessionStart (#289).
    ///
    /// Invoked by the auto-registered SessionStart hook. Outputs a JSON
    /// `hookSpecificOutput.additionalContext` payload so the agent always knows
    /// where its source config lives and won't edit the managed cache directory.
    ConfigContext {
        /// Engine that invoked this hook (e.g. "claude_code"). Accepted for
        /// forward-compatibility; the value is stored for future dispatch use.
        #[arg(long, default_value = "claude_code")]
        engine: String,
    },
    /// Warn when the agent tries to write a managed cache path (#289).
    ///
    /// Invoked by the auto-registered PreToolUse hook (matcher: Write|Edit|MultiEdit).
    /// Reads the tool call from stdin, checks whether the target path is inside
    /// the llmenv cache, and prints a redirection hint. Always exits 0 (fail-soft).
    ConfigGuard {
        /// Engine that invoked this hook (e.g. "claude_code"). Accepted for
        /// forward-compatibility; the value is stored for future dispatch use.
        #[arg(long, default_value = "claude_code")]
        engine: String,
    },
    /// Poll the usage backend and sleep an adaptive delay. See `crate::throttle`.
    Throttle {
        /// Hook event: pre-tool or prompt
        event: String,
    },
    /// Run an agent lifecycle hook (injects ICM memory context over MCP).
    ///
    /// Invoked by the agent runtime, not by users directly. `event` is an
    /// engine-neutral name: session_start | turn_start | session_end.
    HookRun {
        /// Lifecycle event: session_start, turn_start, or session_end
        event: String,
        /// Engine that invoked this hook (e.g. "claude_code"). Accepted for
        /// forward-compatibility; the value is stored for future dispatch use.
        #[arg(long, default_value = "claude_code")]
        engine: String,
    },
    /// Record one session-log event into an ICM transcript session.
    ///
    /// Internal plumbing: this is the detached-child entrypoint
    /// `session_log::detached::spawn_record` launches so a hook process can
    /// return immediately instead of blocking on the transcript MCP call. Not
    /// meant to be invoked directly. Reads the `{session_id, event}` JSON
    /// payload from stdin (the session id travels in the payload rather than
    /// as a CLI argument so it isn't visible in the process table).
    #[command(name = "session-log-record", hide = true)]
    SessionLogRecord,
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

fn run_deprecated_shim(
    old: &str,
    new: &str,
    section: status::StatusSection,
    use_color: bool,
) -> anyhow::Result<()> {
    eprintln!("warning: '{old}' is deprecated; use 'status {new}' instead");
    status::run_status(Some(section), use_color)
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Resolve color emission once: combine the --color flag with stdout TTY
    // state. `export` deliberately never consults this — its stdout is eval'd.
    use std::io::IsTerminal;
    let use_color = should_use_color(Some(cli.color.to_mode()), std::io::stdout().is_terminal());

    match cli.command {
        Some(Command::Doctor { gc, all }) => {
            doctor::run_doctor(gc, all, use_color)?;
        }
        Some(Command::Export {
            scope,
            tag,
            explain,
            compress,
        }) => {
            run_export(scope, tag, explain, compress)?;
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
        Some(Command::Status { section }) => {
            status::run_status(section, use_color)?;
        }
        Some(Command::Context { bundle, why }) => {
            run_context(bundle.as_deref(), why, use_color)?;
        }
        Some(Command::ScopeLs) => {
            run_deprecated_shim(
                "scope-ls",
                "scopes",
                status::StatusSection::Scopes,
                use_color,
            )?;
        }
        Some(Command::TagLs) => {
            run_deprecated_shim("tag-ls", "tags", status::StatusSection::Tags, use_color)?;
        }
        Some(Command::BundleLs) => {
            run_deprecated_shim(
                "bundle-ls",
                "bundles",
                status::StatusSection::Bundles,
                use_color,
            )?;
        }
        Some(Command::McpLs) => {
            run_deprecated_shim("mcp-ls", "mcps", status::StatusSection::Mcps, use_color)?;
        }
        Some(Command::MarketplaceLs) => {
            run_deprecated_shim(
                "marketplace-ls",
                "marketplaces",
                status::StatusSection::Marketplaces,
                use_color,
            )?;
        }
        Some(Command::PluginLs) => {
            run_deprecated_shim(
                "plugin-ls",
                "plugins",
                status::StatusSection::Plugins,
                use_color,
            )?;
        }
        Some(Command::PluginSync) => {
            run_plugin_sync()?;
        }
        Some(Command::Sync { dry_run }) => {
            run_sync(dry_run)?;
        }
        Some(Command::CheckStale { auto_fix, engine }) => {
            tracing::debug!(engine, "check-stale invoked by engine");
            warn_if_unknown_engine(&engine);
            run_check_stale(use_color, auto_fix)?;
        }
        Some(Command::ConfigContext { engine }) => {
            tracing::debug!(engine, "config-context invoked by engine");
            warn_if_unknown_engine(&engine);
            run_config_context();
        }
        Some(Command::ConfigGuard { engine }) => {
            tracing::debug!(engine, "config-guard invoked by engine");
            warn_if_unknown_engine(&engine);
            run_config_guard();
        }
        Some(Command::Throttle { event }) => {
            crate::throttle::run_throttle_hook(&event);
        }
        Some(Command::HookRun { event, engine }) => {
            tracing::debug!(engine, "hook-run invoked by engine");
            warn_if_unknown_engine(&engine);
            crate::hook_run::run(&event)?;
        }
        Some(Command::SessionLogRecord) => {
            use std::io::Read;
            let mut payload_json = String::new();
            std::io::stdin().read_to_string(&mut payload_json)?;
            crate::session_log::detached::run_record(&payload_json)?;
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
        Some(Command::Validate) => {
            run_validate(use_color)?;
        }
        Some(Command::Edit { bundle }) => {
            run_edit(bundle)?;
        }
        Some(Command::Completions { shell }) => {
            run_completions(shell)?;
        }
        None => {
            eprintln!("Usage: llmenv [COMMAND]");
            eprintln!("Run 'llmenv --help' for more information.");
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
    for (name, value) in env {
        validate_var_name(name)?;
        validate_var_value(value)?;
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

fn validate_var_value(value: &str) -> anyhow::Result<()> {
    // Only NUL is rejected: it can't survive in a C-string env var and would
    // truncate the export. Newlines/CR are safe because every emission path
    // single-quotes the value (shell_escape), and values like
    // LLMENV_ICM_CONTEXT are legitimately multiline. (#469)
    if value.contains('\0') {
        anyhow::bail!("variable value contains forbidden NUL byte");
    }
    Ok(())
}

fn run_export(
    scope: Option<String>,
    tag: Option<String>,
    explain: bool,
    compress: bool,
) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let config_dir = paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    // When the memory backend designates *this* host as its server, ensure the
    // local `mcp-proxy` is alive before agents try to reach it. Failures here
    // are logged but non-fatal — the export must still emit env vars so the
    // shell hook stays usable.
    // NOTE: only top-level config.features.memory is used here; bundle-contributed
    // memory entries are merged later in build_manifest and affect resolve_mcps but
    // not the proxy startup check (#335).
    let top_memory: &[_] = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    if let Some(bind) = local_memory_server_bind(top_memory, &active) {
        match crate::mcp::proxy::default_pid_path() {
            Ok(pid_path) => {
                match crate::mcp::proxy::ensure_running(
                    &bind,
                    &pid_path,
                    crate::mcp::proxy::spawn_mcp_proxy,
                ) {
                    Ok(outcome) => {
                        // Warn when binding to all interfaces only on startup — the ICM
                        // daemon is unauthenticated.
                        if outcome == crate::mcp::proxy::EnsureOutcome::Spawned
                            && let Some(mem) = find_local_memory_entry(top_memory, &active)
                            && let Ok(addr) = mem.listen_host.parse::<std::net::IpAddr>()
                            && addr.is_unspecified()
                        {
                            eprintln!(
                                "warning: memory.listen_host is '{}' — the ICM proxy \
                                 will accept connections on ALL network interfaces. \
                                 Set a specific IP to restrict access.",
                                mem.listen_host
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("warning: failed to ensure mcp-proxy running: {e}");
                    }
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
        tracing::warn!("throttled pull failed (non-fatal): {e}");
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
                && !b.when.contains(t)
            {
                return false;
            }
            b.when.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    let mut vars = std::collections::BTreeMap::new();

    // TODO: Scope filtering would require evaluating scope match conditions
    // (network gateway/ssid/cidr, host hostname, user user, project path/marker)
    // For now, only tag filtering is implemented
    if scope.is_some() {
        eprintln!("warning: scope filtering not yet implemented, exporting all matching tags");
    }

    // Merge + materialize the agent config directory for each installed adapter
    // and collect env vars. PATH-gating skips adapters whose binary is absent so
    // a machine without a given tool sees zero behavior change. (#502)
    // NOTE: build_and_materialize is still Claude Code–specific; #506 wires Crush.
    let cache_dir_root = expand_tilde(&config.cache.cache_dir)?;
    for adapter in crate::adapter::registered_adapters() {
        debug_assert_eq!(
            adapter.name(),
            "claude-code",
            "build_and_materialize is Claude-specific until #506 threads the adapter"
        );
        if !crate::adapter::binary_on_path(adapter.binary_name()) {
            tracing::debug!(
                adapter = adapter.name(),
                binary = adapter.binary_name(),
                "adapter binary not on PATH — skipping"
            );
            continue;
        }
        match build_and_materialize(&config, &config_dir, &active, &firing, compress) {
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
                let adapter_root = cache_dir_root.join(adapter.name());
                match crate::materialize::manifest::CacheManifest::read(cache_path) {
                    Ok(Some(mut manifest)) => {
                        crate::auth::detect::sync_auth_on_export(
                            cache_path,
                            &adapter_root,
                            &mut manifest,
                        );
                    }
                    Ok(None) => {} // absent/corrupt: already logged inside CacheManifest::read
                    Err(e) => {
                        tracing::warn!(
                            "auth sync skipped: could not read cache manifest at {}: {e}",
                            cache_path.display()
                        );
                    }
                }
            }
            Ok(None) => {
                tracing::debug!(
                    adapter = adapter.name(),
                    "no bundle content directories — skipping materialize"
                );
            }
            Err(e) => return Err(e).context("agent materialization failed"),
        }
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

    if explain {
        let bundle_list = firing
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        for (key, value) in vars {
            validate_var_name(&key).with_context(|| format!("variable '{key}'"))?;
            validate_var_value(&value)
                .with_context(|| format!("variable '{key}': invalid value"))?;
            if key.starts_with("LLMENV_") {
                println!("# source: llmenv introspection");
            } else {
                println!("# source: adapter (bundles: {bundle_list})");
            }
            println!("export {}={}", key, shell_escape(&value));
        }
    } else {
        for (key, value) in vars {
            validate_var_name(&key).with_context(|| format!("variable '{key}'"))?;
            validate_var_value(&value)
                .with_context(|| format!("variable '{key}': invalid value"))?;
            println!("export {}={}", key, shell_escape(&value));
        }
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
            b.when.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    // Materialize the config
    match build_and_materialize(&config, &config_dir, &active, &firing, false) {
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

/// Compress agents_md by removing excess whitespace and blank lines.
/// Preserves trailing newline (POSIX text file convention).
/// ponytail: simple rule-based compression; use claude -p for higher-quality compression.
fn compress_agents_md(text: &str) -> String {
    let has_trailing_newline = text.ends_with('\n');
    let mut result = text
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    // Collapse 3+ consecutive newlines to 2 (preserves paragraph breaks).
    // Loop until no more `\n\n\n` sequences exist (handles 5+ consecutive newlines).
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    // Restore trailing newline if original had one (POSIX convention).
    if has_trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Returns `true` when an entry with the given `when` tags should be active
/// for the current scope. Empty `when` = always active (no filtering).
fn tag_active(when: &[String], active: &std::collections::BTreeSet<String>) -> bool {
    when.is_empty() || when.iter().any(|t| active.contains(t))
}

/// Emit a warning when `engine` is not the identity of any registered adapter.
/// Defensive: the set of adapters is small and static today; this surfaces
/// stale or mis-typed `--engine` flags before they silently produce wrong output.
///
/// The `--engine` flag uses the underscore form of an adapter's name (the same
/// convention as the `native_<engine>` keys, e.g. `claude_code`), while
/// [`crate::adapter::AgentAdapter::name`] is the hyphenated cache-dir form
/// (`claude-code`). Normalise before comparing so the baked-in default matches.
fn warn_if_unknown_engine(engine: &str) {
    let known = crate::adapter::registered_adapters();
    let engine_id = |a: &dyn crate::adapter::AgentAdapter| a.name().replace('-', "_");
    if !known.iter().any(|a| engine_id(a.as_ref()) == engine) {
        tracing::warn!(
            engine,
            "unrecognised engine name — no registered adapter matches; \
             did you mean one of: {}?",
            known
                .iter()
                .map(|a| engine_id(a.as_ref()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Build BundleRefs for firing bundles in scope-precedence order, merge them
/// into a manifest, materialize through the Claude Code adapter, and return
/// the env vars the adapter wants exported. Returns `Ok(None)` when no
/// firing bundle has a content directory on disk.
fn build_and_materialize(
    config: &Config,
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
    compress: bool,
) -> anyhow::Result<Option<Materialized>> {
    let Some((mut manifest, cache_root)) =
        build_manifest(config, config_dir, active, firing, false)?
    else {
        // No content dirs — clear any stale throttle state so a since-removed
        // throttle config doesn't keep throttling.
        if let Err(e) = crate::throttle::store_active_throttle(None) {
            tracing::debug!("failed to clear throttle state (non-fatal): {e}");
        }
        return Ok(None);
    };

    // Filter skills and lsp entries by active tags — mirror the mcp resolution
    // model: empty `when` means always active; non-empty must intersect active.tags.
    let tags = &active.tags;
    manifest
        .capabilities
        .skills
        .retain(|s| tag_active(&s.when, tags));
    manifest
        .capabilities
        .lsp
        .retain(|l| tag_active(&l.when, tags));

    // Store resolved throttle (top-level + bundle) for hook retrieval.
    if let Err(e) = crate::throttle::store_active_throttle(manifest.throttle.as_ref()) {
        tracing::debug!("failed to store throttle state (non-fatal): {e}");
    }

    // Apply compression if requested: strip trailing whitespace and collapse triple blank lines.
    if compress {
        manifest.agents_md = compress_agents_md(&manifest.agents_md);
    }

    // The selection *shape* (#246) addresses the folder in loose/normal mode:
    // active tags ∪ directly-enabled bundles. Bundles come from active scopes'
    // `enable_bundles` (the manually-forced selection), kept separate from tags
    // so the two namespaces can't alias into one shape.
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

    // Run the adapter writer — materialize writes CLAUDE.md, rules, MCP config,
    // and settings.json. Returns the paths it owns; we union them with the
    // generic bundle files to form llmenv's complete owned set (#196). Idempotent.
    let adapter_owned = adapter.materialize(&manifest, &cache_path)?;

    // Merge user-elected seeded settings after materialize succeeds (#172).
    // Must run post-materialize so a materialize failure leaves settings.json
    // either absent (new folder) or in its prior good state (re-render).
    crate::adapter::claude_code::apply_seeded_settings(&cache_path, &config.init.seeded_settings)?;
    // Seed installMethod to suppress the "config install method is 'unknown'" warning (#346).
    crate::adapter::claude_code::seed_install_method(&cache_path)?;

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

    // Collect env vars from merged bundle Capabilities. Later contributors override
    // earlier ones (enforced by the merge_capabilities function via precedence).
    for (key, value) in &manifest.capabilities.env {
        env_vars.push((key.clone(), value.clone()));
    }

    // Durable state (#175): the state dir is a stable sibling of the hashed
    // config folders (`<adapter_root>/state`), so it survives every hash change.
    // Emit LLMENV_STATE_DIR plus each configured tool's relocation var, and
    // create the dirs so tools find them on first run.
    // When context-mode is enabled (#490) inject CONTEXT_MODE_DATA_DIR as a
    // synthetic StateTool so the store lands in the durable dir automatically.
    let state_cfg = crate::materialize::state::effective_state_config(
        &config.state,
        config.context_mode_enabled(),
    );
    let state_dir = crate::materialize::state::state_dir(&adapter_root);
    crate::materialize::state::ensure_state_dirs(&state_cfg, &state_dir)
        .context("creating durable state directories")?;
    env_vars.extend(crate::materialize::state::state_env_vars(
        &state_cfg, &state_dir,
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
                tracing::warn!("auth inject failed for {} (non-fatal): {e}", entry.email);
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

    // Combine top-level memory + bundle-contributed memory for resolution.
    let top_memory = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let bundle_memory = manifest
        .capabilities
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();
    let mut all_memory: Vec<crate::config::Memory> = top_memory
        .iter()
        .chain(bundle_memory.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_memory);

    // Combine host tables: bundle contributions first, top-level wins on collision.
    let mut all_host = manifest.capabilities.host.clone();
    for (k, v) in &config.host {
        all_host.insert(k.clone(), v.clone());
    }

    manifest.mcps =
        crate::mcp::resolve::resolve_mcps(&config.mcp, &all_memory, &all_host, &active.tags)
            .context("resolving MCP servers")?;
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
    manifest.plugins = sync_plugin_payloads(&cache_root, resolved.plugins);
    manifest.marketplaces = sync_marketplaces(
        config,
        &cache_root,
        resolved.marketplaces,
        refresh_marketplaces,
    )?;

    // Resolve the active throttle entry (tag intersection, single-active).
    let top_throttle = config
        .features
        .as_ref()
        .map(|f| f.throttle.as_slice())
        .unwrap_or_default();
    let bundle_throttle = manifest
        .capabilities
        .features
        .as_ref()
        .map(|f| f.throttle.as_slice())
        .unwrap_or_default();
    let mut all_throttle: Vec<crate::config::Throttle> = top_throttle
        .iter()
        .chain(bundle_throttle.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_throttle);
    manifest.throttle = crate::throttle::resolve_active_throttle(&all_throttle, &active.tags)
        .context("resolving throttle config")?;

    manifest.session_log = config.session_log_resolved();

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
fn run_check_stale(use_color: bool, auto_fix: bool) -> anyhow::Result<()> {
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
            b.when.iter().any(|bt| active.tags.contains(bt))
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
            if auto_fix {
                match build_and_materialize(&config, &config_dir, &active, &firing, false) {
                    Ok(Some((cache_path, _))) => {
                        eprintln!("✓ Config refreshed at {}", cache_path.display());
                    }
                    Ok(None) => {
                        eprintln!("✓ Config up-to-date (no content directory)");
                    }
                    Err(e) => return Err(e).context("auto-fix: re-materialization failed"),
                }
            } else {
                let warn = doctor_warning(use_color);
                eprintln!(
                    "{warn} llmenv config changed in place; restart your agent to load it. \
                     (Bundles, MCP wiring, or plugin paths changed since this session started.)"
                );
            }
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
    use std::io::Read;

    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv config-context: failed to read stdin: {e}");
    }
    let hook_event_name = serde_json::from_str::<serde_json::Value>(&stdin_buf)
        .ok()
        .and_then(|v| v["hook_event_name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "SessionStart".to_owned());

    let emit = |ctx: &str| {
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": ctx
                }
            })
        );
    };

    let config_path = match paths::config_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("llmenv config-context: failed to resolve config path: {e}");
            emit(
                "llmenv config-context: could not resolve config path. \
                 Run `llmenv doctor` to diagnose.",
            );
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
    emit(&text);
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

/// Sync one marketplace and fill `rm.install_location` + `rm.head`.
/// Returns `Some(rm)` on success, `None` when the marketplace isn't cloned
/// yet and `refresh` is false (warn-and-skip, #282).
fn sync_one_marketplace(
    cache_root: &Path,
    market: &crate::config::Marketplace,
    mut rm: crate::plugins::resolve::ResolvedMarketplace,
    refresh: bool,
) -> anyhow::Result<Option<crate::plugins::resolve::ResolvedMarketplace>> {
    match crate::plugins::cache::sync_marketplace(cache_root, market, refresh) {
        Ok(state) => {
            rm.install_location = Some(state.install_location.to_string_lossy().into_owned());
            rm.head = state.head;
            Ok(Some(rm))
        }
        // (#282) During export (refresh=false), a marketplace that isn't cloned
        // locally should not abort materialization — warn and skip so
        // CLAUDE_CONFIG_DIR can still be emitted. run_plugin_sync (refresh=true)
        // still propagates: an explicit sync that can't reach the remote is a
        // real failure the user needs to see.
        Err(crate::plugins::cache::SyncError::NotCloned { .. }) => {
            eprintln!(
                "warning: marketplace '{}' not yet cloned\n  → plugins from this marketplace \
                 are excluded; run `llmenv plugin-sync` to fetch it",
                rm.name
            );
            Ok(None)
        }
        Err(e) => Err(anyhow::anyhow!("syncing marketplace '{}': {e}", rm.name)),
    }
}

/// Sync each resolved marketplace into the shared cache and fill in its
/// `install_location` + `head`. `refresh` controls whether git sources are
/// network-refreshed (`plugin sync`) or used as-is (`export`).
///
/// Resolved marketplaces not present in `config.marketplace` are built-in
/// injections (e.g. context-mode when `features.context_mode.enabled`). They
/// carry their own source URL and are synced via the same logic as declared ones.
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
    for rm in resolved {
        // For declared marketplaces use the config entry; for built-in injected
        // ones (e.g. context-mode) build a transient Marketplace from the
        // resolved source so they are synced rather than silently passed through.
        let transient;
        let market: &crate::config::Marketplace = match by_name.get(rm.name.as_str()) {
            Some(m) => m,
            None => {
                transient = crate::config::Marketplace {
                    name: rm.name.clone(),
                    source: rm.source.clone(),
                };
                &transient
            }
        };
        if let Some(synced) = sync_one_marketplace(cache_root, market, rm, refresh)? {
            out.push(synced);
        }
    }
    Ok(out)
}

/// Look up stable external plugin payload paths for resolved plugins. Non-refreshing
/// (export path): silently skips plugins whose payload hasn't been synced yet, so a
/// missing payload doesn't abort materialization — users run `llmenv plugin-sync` first.
fn sync_plugin_payloads(
    cache_root: &Path,
    plugins: Vec<crate::plugins::resolve::ResolvedPlugin>,
) -> Vec<crate::plugins::resolve::ResolvedPlugin> {
    plugins
        .into_iter()
        .map(|mut p| {
            let mkt_path = crate::plugins::cache::marketplace_path(cache_root, &p.marketplace);
            let Ok(entries) = crate::plugins::cache::read_marketplace_plugins(&mkt_path) else {
                tracing::warn!(
                    "cannot read marketplace manifest for '{}' — skipping external plugin '{}', \
                     run `llmenv plugin-sync` to repair",
                    p.marketplace,
                    p.plugin
                );
                return p;
            };
            let Some(entry) = entries.iter().find(|e| e.name == p.plugin) else {
                tracing::warn!(
                    "plugin '{}' not found in marketplace '{}' manifest — \
                     verify plugin name or run `llmenv plugin-sync` to refresh the clone",
                    p.plugin,
                    p.marketplace
                );
                return p;
            };
            if !crate::plugins::cache::is_external_plugin_source(&entry.source) {
                return p;
            }
            match crate::plugins::cache::sync_external_plugin(
                cache_root,
                &p.marketplace,
                &p.plugin,
                &entry.source,
                false,
            ) {
                Ok(state) => {
                    p.install_path = Some(state.install_location.to_string_lossy().into_owned());
                    p.git_commit_sha = state.head;
                }
                Err(crate::plugins::cache::SyncError::NotCloned { .. }) => {
                    eprintln!(
                        "warning: external plugin '{}@{}' not yet fetched\n  \
                         → run `llmenv plugin-sync` to download it",
                        p.plugin, p.marketplace
                    );
                }
                Err(e) => {
                    eprintln!(
                        "warning: external plugin '{}@{}' payload lookup failed: {e}",
                        p.plugin, p.marketplace
                    );
                }
            }
            p
        })
        .collect()
}

/// Resolve firing bundles to on-disk `BundleRef`s in scope precedence order
/// (network → host → user → project), then unscoped tags in declaration
/// order. Bundles with no content directory under `<config_dir>/bundles/<name>/`
/// are dropped silently — tag-only bundles (no content directory) are valid.
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

    let template = crate::config::generate_template();
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
        run_init_settings_prompt(&config_path)?;
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
        .items([
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
fn run_init_settings_prompt(config_path: &Path) -> anyhow::Result<()> {
    use crate::adapter::claude_code::LLMENV_OWNED_SETTINGS_KEYS;
    use dialoguer::MultiSelect;

    let global_settings = PathBuf::from(paths::expand_tilde("~/.claude")).join("settings.json");
    if !global_settings.exists() {
        return Ok(());
    }
    let bytes = match std::fs::read(&global_settings) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "warning: could not read {} — skipping settings import: {e}",
                global_settings.display()
            );
            return Ok(());
        }
    };
    let doc: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warning: {} is not valid JSON — skipping settings import: {e}",
                global_settings.display()
            );
            return Ok(());
        }
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
    paths::write_owner_only_atomic(config_path, yaml.as_bytes())
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
        // Only inject into a folder that llmenv manages (i.e. under adapter_root).
        // Reject CLAUDE_CONFIG_DIR pointing at an arbitrary directory — that would
        // write auth tokens + a manifest dotfile somewhere unexpected.
        let current_folder = std::env::var("CLAUDE_CONFIG_DIR")
            .ok()
            .map(PathBuf::from)
            .filter(|p| p.starts_with(&adapter_root));
        if current_folder.is_none() {
            eprintln!(
                "note: CLAUDE_CONFIG_DIR is not set or not under the llmenv adapter root — \
                 capturing global auth only. Run `llmenv export` then re-run without \
                 --global to apply to the current folder."
            );
        }
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

    if let Some(folder) = current_folder
        && folder.is_dir()
    {
        crate::auth::inject_auth_into_claude_json(folder, &entry)?;
        // Update manifest auth_status to Explicit.
        if let Ok(Some(mut manifest)) = crate::materialize::manifest::CacheManifest::read(folder) {
            manifest.auth_status = crate::materialize::manifest::AuthStatus {
                source: crate::materialize::manifest::AuthSource::Explicit,
                id: Some(entry.uuid),
                email: Some(entry.email),
            };
            manifest.write(folder)?;
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

fn run_context(bundle_filter: Option<&str>, why: bool, use_color: bool) -> anyhow::Result<()> {
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
            let why_str = if why {
                let matched: Vec<&str> = tags
                    .iter()
                    .filter(|t| active.tags.contains(*t))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    String::new()
                } else {
                    format!(" [why: tags={}]", matched.join(","))
                }
            } else {
                String::new()
            };
            active_scopes.push(format!(
                "{} {}{}{}",
                active_marker(use_color),
                name,
                annotation,
                why_str
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

    let bundles_to_show: Vec<&Bundle> = if let Some(filter) = bundle_filter {
        let matching: Vec<&Bundle> = config.bundle.iter().filter(|b| b.name == filter).collect();
        if matching.is_empty() {
            anyhow::bail!("bundle not found: {filter}");
        }
        matching
    } else {
        config.bundle.iter().collect()
    };

    if !bundles_to_show.is_empty() {
        println!("\nBundles");
        for b in &bundles_to_show {
            let is_active = b.when.iter().any(|tag| active.tags.contains(tag));
            let mark = if is_active {
                active_marker(use_color)
            } else {
                " ".to_string()
            };
            let why_str = if why && is_active {
                let matched: Vec<&str> = b
                    .when
                    .iter()
                    .filter(|t| active.tags.contains(*t))
                    .map(String::as_str)
                    .collect();
                format!(" [why: tags={}]", matched.join(","))
            } else {
                String::new()
            };
            println!("{} {}{}", mark, b.name, why_str);
        }
    }

    let firing: Vec<&Bundle> = config
        .bundle
        .iter()
        .filter(|b| b.when.iter().any(|tag| active.tags.contains(tag)))
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
        .flat_map(|b| b.when.iter().cloned())
        .chain(config.mcp.iter().flat_map(|m| m.when.iter().cloned()))
        .chain(
            config
                .features
                .as_ref()
                .iter()
                .flat_map(|f| f.memory.iter())
                .flat_map(|m| m.when.iter().cloned()),
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
    let tag_matches = bundle.when.iter().any(|t| tag_looks_marker_sourced(t));
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

/// Find the memory entry that is both tag-active and designates *this* host as
/// its server. Returns `None` when this host is a memory client, the memory
/// list is empty, or no active entry names this host.
fn find_local_memory_entry<'a>(
    memory: &'a [crate::config::Memory],
    active: &ActiveScopes,
) -> Option<&'a crate::config::Memory> {
    let host_ids = active_host_ids(active);
    memory.iter().find(|m| {
        m.when.iter().any(|t| active.tags.contains(t)) && host_ids.contains(&m.server_host)
    })
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
fn local_memory_server_bind(
    memory: &[crate::config::Memory],
    active: &ActiveScopes,
) -> Option<String> {
    let mem = find_local_memory_entry(memory, active)?;
    Some(format!("{}:{}", mem.listen_host, mem.port))
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

/// Sync every configured marketplace into the shared cache: git sources are
/// cloned on first use and fast-forwarded on subsequent runs; path sources are
/// resolved in place. Reports each marketplace's resolved location + HEAD.
fn run_plugin_sync() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let cache_root = expand_tilde(&config.cache.cache_dir)?;

    let context_mode_enabled = config.context_mode_enabled();

    if config.marketplace.is_empty() && !context_mode_enabled {
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

    // Sync the built-in context-mode marketplace when the feature is enabled
    // and the user has not already declared a context-mode marketplace entry
    // (which would have been handled by the loop above).
    let user_declared_context_mode = config
        .marketplace
        .iter()
        .any(|m| m.name == crate::config::CONTEXT_MODE_MARKETPLACE);
    if context_mode_enabled && !user_declared_context_mode {
        let builtin = crate::config::Marketplace {
            name: crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
            source: crate::config::CONTEXT_MODE_SOURCE.to_string(),
        };
        let state = crate::plugins::cache::sync_marketplace(&cache_root, &builtin, true)
            .with_context(|| format!("syncing built-in marketplace '{}'", builtin.name))?;
        let head = state.head.as_deref().unwrap_or("(local path)");
        println!(
            "✓ {} → {} [{}]",
            builtin.name,
            state.install_location.display(),
            head
        );
    }

    // Sync external plugin payloads: plugins whose source in marketplace.json is
    // an external git URL (not a relative path within the marketplace clone).
    let all_plugin_refs: std::collections::HashSet<(String, String)> = config
        .plugin_collection
        .iter()
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| crate::config::split_plugin_ref(p))
        .map(|(mkt, plugin)| (mkt.to_string(), plugin.to_string()))
        .collect();

    let mut missing_plugins: Vec<String> = Vec::new();
    for (mkt_name, plugin_name) in &all_plugin_refs {
        let mkt_path = crate::plugins::cache::marketplace_path(&cache_root, mkt_name);
        let plugins = crate::plugins::cache::read_marketplace_plugins(&mkt_path)
            .with_context(|| format!("reading marketplace manifest for '{mkt_name}'"))?;
        let Some(entry) = plugins.iter().find(|p| p.name == *plugin_name) else {
            eprintln!(
                "✗ {plugin_name}@{mkt_name}: not found in marketplace manifest after sync — \
                 check that the plugin name matches an entry in {mkt_name}"
            );
            missing_plugins.push(format!("{plugin_name}@{mkt_name}"));
            continue;
        };
        if !crate::plugins::cache::is_external_plugin_source(&entry.source) {
            continue;
        }
        let state = crate::plugins::cache::sync_external_plugin(
            &cache_root,
            mkt_name,
            plugin_name,
            &entry.source,
            true,
        )
        .with_context(|| format!("syncing external plugin '{plugin_name}@{mkt_name}'"))?;
        let head = state.head.as_deref().unwrap_or("(unknown)");
        println!(
            "✓ {}@{} (external) → {} [{}]",
            plugin_name,
            mkt_name,
            state.install_location.display(),
            head
        );
    }
    if !missing_plugins.is_empty() {
        anyhow::bail!(
            "{} plugin(s) not found after sync: {}",
            missing_plugins.len(),
            missing_plugins.join(", ")
        );
    }
    Ok(())
}

fn run_sync(dry_run: bool) -> anyhow::Result<()> {
    let config_dir = paths::config_dir()?;
    if dry_run {
        let out = git::secure_git()
            .args(["status", "--short"])
            .current_dir(&config_dir)
            .output()
            .context("failed to run git status")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("git status failed: {stderr}");
        }
        let output = String::from_utf8_lossy(&out.stdout);
        if output.trim().is_empty() {
            eprintln!("Nothing to sync (working tree clean)");
        } else {
            eprintln!("Would sync:\n{output}");
        }
        return Ok(());
    }
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

fn run_validate(use_color: bool) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let pass = doctor_pass(use_color);
    let fail = doctor_fail(use_color);
    let mut valid = true;
    let mut seen_names = HashSet::new();
    for bundle in &config.bundle {
        if !seen_names.insert(bundle.name.as_str()) {
            eprintln!("{fail} duplicate bundle name: {}", bundle.name);
            valid = false;
        }
    }
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    for scope in &active.scopes {
        if scope.kind != "project" {
            continue;
        }
        for bundle_name in &scope.enable_bundles {
            if !seen_names.contains(bundle_name.as_str()) {
                eprintln!(
                    "{fail} .llmenv.yaml enable_bundles references unknown bundle: {bundle_name}"
                );
                valid = false;
            }
        }
    }
    if valid {
        eprintln!("{pass} config valid ({} bundle(s))", config.bundle.len());
    } else {
        anyhow::bail!("validation failed");
    }
    Ok(())
}

fn run_edit(bundle: Option<String>) -> anyhow::Result<()> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_owned());
    let path = if let Some(name) = bundle {
        if crate::paths::is_unsafe_join_target(&name) {
            anyhow::bail!("unsafe bundle name: {name}");
        }
        let config_dir = paths::config_dir()?;
        let candidate = config_dir.join("bundles").join(format!("{name}.yaml"));
        if candidate.exists() {
            candidate
        } else {
            let alt = config_dir.join("bundles").join(format!("{name}.yml"));
            if alt.exists() {
                alt
            } else {
                anyhow::bail!("bundle file not found: bundles/{name}.yaml");
            }
        }
    } else {
        paths::config_path()?
    };
    let parts: Vec<&str> = editor.split_whitespace().collect();
    let bin = parts
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("$EDITOR / $VISUAL is set but empty"))?;
    let extra_args = parts.get(1..).unwrap_or_default();
    let status = std::process::Command::new(bin)
        .args(extra_args)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor: {editor}"))?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    Ok(())
}

fn run_completions(shell: clap_complete::Shell) -> anyhow::Result<()> {
    use clap::CommandFactory;
    use std::io::Write;
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "llmenv", &mut buf);
    std::io::stdout()
        .write_all(&buf)
        .context("failed to write completions to stdout")?;
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
    fn compress_agents_md_removes_trailing_whitespace() {
        let input = "line1  \nline2   \nline3";
        let expected = "line1\nline2\nline3";
        assert_eq!(compress_agents_md(input), expected);
    }

    #[test]
    fn compress_agents_md_collapses_triple_blank_lines() {
        let input = "text\n\n\n\n\nmore text";
        let expected = "text\n\nmore text";
        assert_eq!(compress_agents_md(input), expected);
    }

    #[test]
    fn compress_agents_md_preserves_double_blank_lines() {
        let input = "text\n\ndouble\n\nmore";
        assert_eq!(compress_agents_md(input), input);
    }

    #[test]
    fn compress_agents_md_preserves_trailing_newline() {
        let input = "text\n\n\n\nmore\n";
        let expected = "text\n\nmore\n";
        assert_eq!(compress_agents_md(input), expected);
    }

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
        // Newlines/CR stay inside the single-quoted string — inert at shell source time.
        assert_eq!(shell_escape("line1\nline2"), "'line1\nline2'");
        assert_eq!(shell_escape("line1\rline2"), "'line1\rline2'");
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
    fn reject_invalid_var_names_allows_multiline_values() {
        // NUL truncates the C-string env var — still rejected.
        let with_nul = vec![("VALID_NAME".to_string(), "value\0malicious".to_string())];
        assert!(reject_invalid_var_names(&with_nul).is_err());

        // Newlines/CR are safe — emission always single-quotes via shell_escape. (#469)
        let with_newline = vec![(
            "VALID_NAME".to_string(),
            "## context\nActive tags: `foo`".to_string(),
        )];
        assert!(reject_invalid_var_names(&with_newline).is_ok());

        let with_cr = vec![("VALID_NAME".to_string(), "value\rmore".to_string())];
        assert!(reject_invalid_var_names(&with_cr).is_ok());
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
                memory: vec![Memory {
                    server_host: "srv".to_string(),
                    port,
                    listen_host: listen_host.to_string(),
                    when: vec!["mem".to_string()],
                    default_topics: vec![],
                }],
                throttle: vec![],
                context_mode: None,
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

    fn config_memory(config: &Config) -> &[crate::config::Memory] {
        config
            .features
            .as_ref()
            .map(|f| f.memory.as_slice())
            .unwrap_or_default()
    }

    #[test]
    fn local_memory_server_bind_defaults_to_loopback() {
        // Default listen_host must be 127.0.0.1 — backward-compatible loopback.
        let config = memory_config("127.0.0.1", 7878);
        let active = active_as_server();
        let bind = local_memory_server_bind(config_memory(&config), &active);
        assert_eq!(bind, Some("127.0.0.1:7878".to_string()));
    }

    #[test]
    fn local_memory_server_bind_honours_custom_host() {
        // A configured listen_host is forwarded into the bind address.
        let config = memory_config("0.0.0.0", 9000);
        let active = active_as_server();
        let bind = local_memory_server_bind(config_memory(&config), &active);
        assert_eq!(bind, Some("0.0.0.0:9000".to_string()));
    }

    #[test]
    fn local_memory_server_bind_returns_none_for_client_host() {
        // When this host is not the designated server, no bind address is returned.
        let config = memory_config("127.0.0.1", 7878);
        let active = active_as_client();
        let bind = local_memory_server_bind(config_memory(&config), &active);
        assert_eq!(bind, None);
    }

    #[test]
    fn local_memory_server_bind_returns_none_when_memory_unconfigured() {
        let config = Config::default();
        let active = active_as_server();
        let bind = local_memory_server_bind(config_memory(&config), &active);
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

    #[test]
    fn sync_marketplaces_injected_builtin_is_synced_not_silently_skipped() {
        // Regression test for #490: a resolved marketplace that is NOT in
        // config.marketplace (i.e. the injected context-mode built-in) must be
        // synced — not silently passed through with install_location=None.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("context-mode");
        std::fs::create_dir(&src).unwrap();

        // config.marketplace is empty — simulates user having only
        // features.context_mode.enabled: true without a manual marketplace entry.
        let config = Config {
            marketplace: vec![],
            ..Config::default()
        };
        let cache = tempfile::tempdir().unwrap();

        // The resolved entry carries the source (as inject_context_mode sets it).
        let rm = crate::plugins::resolve::ResolvedMarketplace {
            name: "context-mode".into(),
            source: src.to_string_lossy().into_owned(),
            install_location: None,
            head: None,
        };

        let result = sync_marketplaces(&config, cache.path(), vec![rm], false);
        assert!(
            result.is_ok(),
            "injected built-in should sync without error"
        );
        let out = result.unwrap();
        assert_eq!(out.len(), 1, "injected marketplace must appear in output");
        assert!(
            out[0].install_location.is_some(),
            "install_location must be filled in for injected built-in (was None before fix)"
        );
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
