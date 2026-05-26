use crate::config::Config;
use crate::paths;
use anyhow::Context;
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "llmenv",
    version,
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
    ScopeLs,
    /// List available tags
    TagLs,
    /// List available bundles
    BundleLs,
    /// Sync config with GitHub (git add, commit, push)
    Sync,
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

    // When this host's active scope tags include `icm.server_tag`, ensure
    // the local `mcp-proxy` is alive before agents try to reach it. Failures
    // here are logged but non-fatal — the export must still emit env vars so
    // the shell hook stays usable.
    if let Some(icm) = &config.icm {
        let env = crate::scope::matcher::Env::detect();
        let active = crate::scope::evaluate(&config, &env);
        if active.tags.contains(&icm.server_tag) {
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
    }

    // Throttled pull: check sync interval and fetch+pull if enough time has elapsed
    let interval_secs = config.settings.sync_interval_minutes * 60;
    let state_dir = paths::state_dir()?;
    let config_dir = paths::config_dir()?;
    if let Err(e) = crate::sync::maybe_pull(
        &config_dir,
        &state_dir,
        std::time::Duration::from_secs(interval_secs),
    ) {
        tracing::debug!("throttled pull failed (non-fatal): {e}");
    }

    let mut vars = std::collections::BTreeMap::new();
    for bundle in &config.bundle {
        if let Some(ref t) = tag
            && !bundle.tags.contains(t)
        {
            continue;
        }
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

    for (key, value) in vars {
        validate_var_name(&key)?;
        println!("export {}={}", key, shell_escape(&value));
    }

    Ok(())
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

fn run_init(
    path: Option<std::path::PathBuf>,
    repo: Option<String>,
) -> anyhow::Result<()> {
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

fn run_scope_ls() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;

    let mut scopes = Vec::new();
    for scope in &config.scope.network {
        scopes.push(format!("network:{}", scope.id));
    }
    for scope in &config.scope.host {
        scopes.push(format!("host:{}", scope.id));
    }
    for scope in &config.scope.user {
        scopes.push(format!("user:{}", scope.id));
    }
    for scope in &config.scope.project {
        scopes.push(format!("project:{}", scope.id));
    }

    scopes.sort();
    for scope in scopes {
        println!("{}", scope);
    }

    Ok(())
}

fn run_tag_ls() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;

    let mut tags = HashSet::new();
    for scope in &config.scope.network {
        tags.extend(scope.tags.iter().cloned());
    }
    for scope in &config.scope.host {
        tags.extend(scope.tags.iter().cloned());
    }
    for scope in &config.scope.user {
        tags.extend(scope.tags.iter().cloned());
    }
    for scope in &config.scope.project {
        tags.extend(scope.tags.iter().cloned());
    }
    for bundle in &config.bundle {
        tags.extend(bundle.tags.iter().cloned());
    }

    let mut tag_list: Vec<_> = tags.into_iter().collect();
    tag_list.sort();
    for tag in tag_list {
        println!("{}", tag);
    }

    Ok(())
}

fn run_bundle_ls() -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;

    let mut bundles: Vec<_> = config.bundle.iter().map(|b| b.name.clone()).collect();
    bundles.sort();
    for bundle in bundles {
        println!("{}", bundle);
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
        let home = std::env::var("HOME").unwrap();
        let result = expand_tilde("~/test").unwrap();
        assert_eq!(result, PathBuf::from(format!("{}/test", home)));
    }

    #[test]
    fn expand_tilde_tilde_only() {
        let home = std::env::var("HOME").unwrap();
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
