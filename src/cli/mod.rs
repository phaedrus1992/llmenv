use crate::config::Config;
use crate::paths;
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "llme",
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
        Some(Command::Init { repo }) => {
            run_init(repo)?;
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
        None => {
            eprintln!("Usage: llme [COMMAND]");
            eprintln!("Run 'llme --help' for more information.");
        }
    }

    Ok(())
}

/// Validates adapter wiring: file layout, config parse, no silent breakage.
fn run_doctor(gc: bool) -> anyhow::Result<()> {
    eprintln!("Running llme doctor...");

    // Check that CLAUDE_CONFIG_DIR is set (if in an active scope)
    if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        eprintln!("✓ CLAUDE_CONFIG_DIR set to: {}", config_dir);

        let config_path = PathBuf::from(&config_dir);

        // Validate that adapter wiring exists
        let claude_md = config_path.join("CLAUDE.md");
        if claude_md.exists() {
            eprintln!("✓ CLAUDE.md found");
        } else {
            eprintln!("⚠ CLAUDE.md not found at {}", claude_md.display());
        }

        let settings_json = config_path.join("settings.json");
        if settings_json.exists() {
            eprintln!("✓ settings.json found");
            // Try to parse it
            let content = std::fs::read_to_string(&settings_json)?;
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(_) => eprintln!("✓ settings.json is valid JSON"),
                Err(e) => {
                    eprintln!("✗ settings.json parse error: {}", e);
                    return Err(e.into());
                }
            }
        } else {
            eprintln!("⚠ settings.json not found at {}", settings_json.display());
        }

        let skills_dir = config_path.join("skills");
        if skills_dir.exists() {
            match std::fs::read_dir(&skills_dir) {
                Ok(entries) => {
                    let skill_count = entries.count();
                    eprintln!("✓ skills/ directory exists ({} items)", skill_count);
                }
                Err(e) => {
                    eprintln!("✗ Failed to read skills/ directory: {}", e);
                    return Err(e.into());
                }
            }
        }
    } else {
        eprintln!("⚠ CLAUDE_CONFIG_DIR not set (not in an active scope?)");
    }

    eprintln!("Doctor check complete.");

    if gc {
        eprintln!("✓ GC flag set (cache cleanup deferred to full implementation)");
    }

    Ok(())
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
        anyhow::bail!("Variable name '{}' must start with letter or underscore", name);
    }
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            anyhow::bail!("Variable name '{}' contains invalid character '{}'", name, ch);
        }
    }
    Ok(())
}

fn run_export(scope: Option<String>, tag: Option<String>) -> anyhow::Result<()> {
    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;

    let mut vars = std::collections::BTreeMap::new();
    for bundle in &config.bundle {
        // Filter by tags if specified
        if let Some(ref t) = tag {
            if !bundle.tags.contains(t) {
                continue;
            }
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
            println!("__llme_precmd() {{");
            println!("  source <(llme export)");
            println!("}}");
            println!();
            println!("# Add to precmd_functions if not already present");
            println!("if [[ ! \" ${{precmd_functions[@]}} \" =~ \" __llme_precmd \" ]]; then");
            println!("  precmd_functions+=(\"__llme_precmd\")");
            println!("fi");
        }
        "bash" => {
            println!("__llme_prompt() {{");
            println!("  source <(llme export)");
            println!("}}");
            println!();
            println!("# Prepend to PROMPT_COMMAND if not already present");
            println!("if [[ \"$PROMPT_COMMAND\" != *\"__llme_prompt\"* ]]; then");
            println!("  PROMPT_COMMAND=\"__llme_prompt;$PROMPT_COMMAND\"");
            println!("fi");
        }
        _ => {
            anyhow::bail!("Unsupported shell: {}. Supported: zsh, bash", shell);
        }
    }

    Ok(())
}

fn run_init(repo: Option<String>) -> anyhow::Result<()> {
    let config_dir = paths::config_dir()?;
    std::fs::create_dir_all(&config_dir)?;

    if let Some(_repo_url) = repo {
        anyhow::bail!("Git clone not yet implemented");
    } else {
        let config_path = config_dir.join("config.toml");
        if !config_path.exists() {
            let template = r#"[settings]
cache_dir = "~/.cache/llmenv"
sync_interval_minutes = 60

[scope.network]

[scope.host]

[scope.user]

[scope.project]

[[bundle]]
name = "base"
tags = []

[bundle.vars]
"#;
            std::fs::write(&config_path, template)?;
            eprintln!("Created template config at {}", config_path.display());
        }
    }

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
}
