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

    match cli.command {
        Some(Command::Doctor { gc }) => {
            run_doctor(gc)?;
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
            run_status()?;
        }
        Some(Command::ScopeLs) => {
            run_scope_ls()?;
        }
        Some(Command::TagLs) => {
            run_tag_ls()?;
        }
        Some(Command::BundleLs) => {
            run_bundle_ls()?;
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
fn run_doctor(gc: bool) -> anyhow::Result<()> {
    eprintln!("Running llmenv doctor...");

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    eprintln!("✓ Configuration loaded from {}", config_path.display());

    // Check that config parses
    eprintln!("✓ Config is valid TOML");

    // Check cache directory is writable
    let cache_dir = expand_tilde(&config.settings.cache_dir)?;
    std::fs::create_dir_all(&cache_dir).context("cache directory not writable")?;
    eprintln!("✓ Cache directory is writable: {}", cache_dir.display());

    // Check git remote is reachable (if config_dir is a git repo)
    let config_dir = paths::config_dir()?;
    if is_git_repo(&config_dir) {
        match check_git_remote(&config_dir) {
            Ok(remote) => {
                let safe_url = sanitize_git_url(&remote);
                eprintln!("✓ Git remote reachable: {}", safe_url);
            }
            Err(e) => eprintln!("⚠ Git remote check failed: {}", e),
        }
    } else {
        eprintln!("⚠ Config directory is not a git repo");
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
                "⚠ orphan scope network:{}: no bundle consumes its tags",
                s.id
            );
            orphan_count += 1;
        }
    }
    for s in &config.scope.host {
        if !s.tags.iter().any(|t| consumed.contains(t)) {
            eprintln!("⚠ orphan scope host:{}: no bundle consumes its tags", s.id);
            orphan_count += 1;
        }
    }
    for s in &config.scope.user {
        if !s.tags.iter().any(|t| consumed.contains(t)) {
            eprintln!("⚠ orphan scope user:{}: no bundle consumes its tags", s.id);
            orphan_count += 1;
        }
    }
    for s in &config.scope.project {
        if !s.tags.iter().any(|t| consumed.contains(t)) {
            eprintln!(
                "⚠ orphan scope project:{}: no bundle consumes its tags",
                s.id
            );
            orphan_count += 1;
        }
    }
    for b in &config.bundle {
        let has_emitted_tag = b.tags.iter().any(|t| emitted.contains(t));
        if !has_emitted_tag && !marker_enabled.contains(&b.name) {
            eprintln!(
                "⚠ orphan bundle {}: no scope emits its tags and no marker enables it",
                b.name
            );
            orphan_count += 1;
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
        eprintln!("⚠ orphan tag {}: {}", t, reason);
        orphan_count += 1;
    }

    if orphan_count == 0 {
        eprintln!("✓ No orphan scopes/tags/bundles");
    } else {
        eprintln!("⚠ Found {} orphan item(s)", orphan_count);
    }

    eprintln!("✓ Doctor check complete.");

    if gc {
        eprintln!("Running garbage collection...");
        match std::fs::metadata(&cache_dir) {
            Ok(meta) => {
                if meta.permissions().readonly() {
                    eprintln!("⚠ GC failed: cache directory is read-only");
                } else {
                    let cache_retention_hours =
                        config.settings.cache_retention_hours.unwrap_or(168);
                    let retention = std::time::Duration::from_secs(cache_retention_hours * 3600);
                    match crate::materialize::cache::gc(&cache_dir, retention) {
                        Ok(report) => {
                            eprintln!(
                                "✓ GC complete: removed {} entries, kept {}",
                                report.removed.len(),
                                report.kept
                            );
                        }
                        Err(e) => eprintln!("⚠ GC failed: {}", e),
                    }
                }
            }
            Err(e) => eprintln!("⚠ GC failed to stat cache directory: {}", e),
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

    // When this host's active scope tags include `icm.server_tag`, ensure
    // the local `mcp-proxy` is alive before agents try to reach it. Failures
    // here are logged but non-fatal — the export must still emit env vars so
    // the shell hook stays usable.
    if let Some(icm) = &config.icm
        && active.tags.contains(&icm.server_tag)
    {
        match crate::mcp::proxy::default_pid_path() {
            Ok(pid_path) => {
                if let Err(e) = crate::mcp::proxy::ensure_running(
                    &icm.server_bind,
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
    let interval_secs = config.settings.sync_interval_minutes * 60;
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

    let mut manifest: MergedManifest = crate::merge::merge(&refs)?;
    if let Some(icm) = &config.icm {
        manifest.icm = Some(icm.clone());
        manifest.icm_is_server = active.tags.contains(&icm.server_tag);
    }

    let cache_root = expand_tilde(&config.settings.cache_dir)?;
    let adapter = ClaudeCodeAdapter;
    let adapter_root = cache_root.join(adapter.name());
    let cache_path = crate::materialize::materialize(&manifest, &adapter_root)?;

    // Run the adapter writer too — materialize copies raw bundle files, but
    // only the adapter writes the agent-native rules file (CLAUDE.md), the
    // MCP config, and settings.json. Idempotent per the adapter contract.
    adapter.materialize(&manifest, &cache_path)?;

    let env_vars = adapter.env_vars(&cache_path)?;
    Ok(Some((cache_path, env_vars)))
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

    let push_ref = |name: &str, refs: &mut Vec<BundleRef>, seen: &mut BTreeSet<String>| {
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
        });
    };

    for kind in PRECEDENCE {
        // Tags emitted by scopes of this kind.
        let kind_tags: BTreeSet<&str> = active
            .scopes
            .iter()
            .filter(|s| s.kind == *kind)
            .flat_map(|s| s.tags.iter().map(String::as_str))
            .collect();
        for bundle in firing {
            if bundle.tags.iter().any(|t| kind_tags.contains(t.as_str())) {
                push_ref(&bundle.name, &mut refs, &mut seen);
            }
        }
    }
    // Any firing bundle not already placed (shouldn't happen — every firing
    // bundle has at least one tag in active.tags — but defensive).
    for bundle in firing {
        push_ref(&bundle.name, &mut refs, &mut seen);
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

    let config_path = config_dir.join("config.toml");
    if config_path.exists() {
        eprintln!("Config already exists at {}", config_path.display());
        return Ok(());
    }

    let template = r#"[settings]
cache_dir = "~/.cache/llmenv"
sync_interval_minutes = 60

# Scopes are arrays of tables — uncomment and fill in as needed.
# [[scope.network]]
# id = "home"
# match = { ssid = "MyHomeWiFi" }
# tags = ["home"]

# [[scope.host]]
# id = "laptop"
# match = { hostname = "my-laptop" }
# tags = ["laptop"]

# [[scope.user]]
# id = "me"
# match = { user = "alice" }
# tags = ["me"]

# [[scope.project]]
# id = "myapp"
# match = { marker = ".llmenvrc" }
# tags = ["myapp"]

# Bundles fire when one of their tags is emitted by a matching scope.
[[bundle]]
name = "base"
tags = ["me"]

[bundle.vars]
AGENT = "claude"
"#;
    std::fs::write(&config_path, template)
        .with_context(|| format!("writing template to {}", config_path.display()))?;
    eprintln!("Created template config at {}", config_path.display());

    Config::load(&config_path)?;
    eprintln!("✓ Config validated successfully");

    Ok(())
}

fn run_status() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    match Config::load(&config_path) {
        Ok(config) => {
            eprintln!("✓ Configuration loaded from {}", config_path.display());
            eprintln!("  Scopes:");
            eprintln!("    Network: {}", config.scope.network.len());
            eprintln!("    Host: {}", config.scope.host.len());
            eprintln!("    User: {}", config.scope.user.len());
            eprintln!("    Project: {}", config.scope.project.len());
            eprintln!("  Bundles: {}", config.bundle.len());
        }
        Err(e) => {
            eprintln!("✗ Configuration error: {}", e);
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

/// Tags consumed by any configured bundle.
fn all_consumed_tags(config: &Config) -> HashSet<String> {
    config
        .bundle
        .iter()
        .flat_map(|b| b.tags.iter().cloned())
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

/// Annotation suffix for a listing row.
fn annotate(active: bool, orphan: bool) -> &'static str {
    if active {
        ""
    } else if orphan {
        " (orphan)"
    } else {
        " (inactive)"
    }
}

fn run_scope_ls() -> anyhow::Result<()> {
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
        let mark = if is_active { "*" } else { " " };
        println!("{} {}{}", mark, name, annotate(is_active, is_orphan));
    }
    Ok(())
}

fn run_tag_ls() -> anyhow::Result<()> {
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
        let mark = if is_active { "*" } else { " " };
        println!("{} {}{}", mark, tag, annotate(is_active, is_orphan));
    }
    Ok(())
}

fn run_bundle_ls() -> anyhow::Result<()> {
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
        let mark = if is_active { "*" } else { " " };
        println!("{} {}{}", mark, name, annotate(is_active, is_orphan));
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
    // Validate flag combinations
    if all && older_than.is_some() {
        anyhow::bail!("--all and --older-than are mutually exclusive");
    }

    // Validate duration format if provided
    if let Some(duration_str) = &older_than {
        humantime::parse_duration(duration_str)
            .with_context(|| format!("failed to parse --older-than duration: {}", duration_str))?;
    }

    // TODO: Implement prune logic
    // - Call materialize::cache::prune() with appropriate flags
    // - SECURITY: Ensure materialize::cache::prune() validates:
    //   1. All paths stay within cache root (no .. traversal)
    //   2. Symlinks are not followed (or at least validated)
    //   3. --dry-run prevents all writes
    // - Print summary

    eprintln!(
        "prune stub: all={}, older_than={:?}, dry_run={}",
        all, older_than, dry_run
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
