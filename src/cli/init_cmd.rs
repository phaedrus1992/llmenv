//! `llmenv init`: write a new config folder, its starter files, and the first-run prompts.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::adapter::AgentAdapter as _;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::config::Config;
use crate::paths;

use super::init_settings;
use super::init_templates;
use super::run_init_auth_prompt;

/// The folder that `llmenv init` writes into: the `--path` value with `~` expanded, or the default
/// config folder.
pub(super) fn init_config_dir(path: Option<std::path::PathBuf>) -> anyhow::Result<PathBuf> {
    match path {
        Some(p) => {
            let path_str = p
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("init path is not valid UTF-8: {}", p.display()))?;
            Ok(PathBuf::from(paths::expand_tilde(path_str)?))
        }
        None => paths::config_dir(),
    }
}

/// Write a starter file into a new config folder. A file that exists stays as it is. The file is
/// created with `create_new`, so a link or a file that appears after a check is never written
/// through, and the mode is owner-only, like the rest of the config folder.
pub(super) fn write_starter_if_missing(
    path: &Path,
    content: &str,
    label: &str,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(content.as_bytes())
                .with_context(|| format!("writing {label} to {}", path.display()))?;
            eprintln!("Created {label} at {}", path.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("creating {label} at {}", path.display())))
        }
    }
}

pub(super) fn run_init(
    path: Option<std::path::PathBuf>,
    repo: Option<String>,
) -> anyhow::Result<()> {
    // A folder the user named is not owned by llmenv, so its mode is left alone (#3223).
    let user_folder = path.is_some();
    let config_dir = init_config_dir(path)?;
    let created = if user_folder {
        paths::create_dir_owner_only_refusing_unreadable(&config_dir)
    } else {
        paths::create_dir_owner_only(&config_dir)
    };
    created.with_context(|| format!("creating config dir {}", config_dir.display()))?;

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
    write_starter_if_missing(
        &agents_path,
        init_templates::AGENTS_TEMPLATE,
        "agent orientation guide",
    )?;

    let readme_path = config_dir.join("README.md");
    write_starter_if_missing(&readme_path, init_templates::README_TEMPLATE, "README")?;

    // Interactive first-run prompts, only on a terminal: dialoguer reads stdin and asks on stderr.
    // Intentionally Claude Code-only: Crush has no auth/settings concept yet.
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        let adapter_root = PathBuf::from(paths::expand_tilde(&config.cache.cache_dir)?)
            .join(ClaudeCodeAdapter.name());
        run_init_auth_prompt(&adapter_root)?;
        init_settings::run_init_settings_prompt(&config_path)?;
    }

    Ok(())
}
