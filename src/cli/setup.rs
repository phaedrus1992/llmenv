use crate::config::Config;
use crate::paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Detected external tool configuration.
struct DetectedConfig {
    source: String,
    path: PathBuf,
}

/// Scan common locations for existing LLM tool configs.
fn scan_existing_configs() -> Vec<DetectedConfig> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Vec::new(),
    };
    let mut configs = Vec::new();

    // Claude Code
    let claude_dir = home.join(".claude");
    let settings = claude_dir.join("settings.json");
    if settings.is_file() {
        configs.push(DetectedConfig {
            source: "Claude Code settings".to_string(),
            path: settings,
        });
    }

    // Claude Code projects
    let projects_dir = claude_dir.join("projects");
    match std::fs::read_dir(&projects_dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(de) => {
                        let proj_settings = de.path().join("settings.json");
                        if proj_settings.is_file() {
                            configs.push(DetectedConfig {
                                source: format!("Claude Code project: {}", de.path().display()),
                                path: proj_settings,
                            });
                        }
                    }
                    Err(e) => {
                        eprintln!("  warn: skipping unreadable entry in ~/.claude/projects: {e}");
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No projects directory yet, that's fine
        }
        Err(e) => {
            eprintln!("  warn: could not scan ~/.claude/projects: {e}");
        }
    }

    // Cursor
    let cursor_settings = home.join(".cursor").join("settings.json");
    if cursor_settings.is_file() {
        configs.push(DetectedConfig {
            source: "Cursor settings".to_string(),
            path: cursor_settings,
        });
    }

    configs
}

/// Prompt the user to pick bundle names and create bundle directories.
fn prompt_bundles(config_dir: &Path) -> Result<Vec<String>> {
    use dialoguer::Input;

    let mut bundles = Vec::new();
    eprintln!();
    eprintln!("Let's set up your bundles.");
    eprintln!("Bundles group related configuration (e.g. home, work, common languages).");

    loop {
        let name: String = Input::new()
            .with_prompt("Bundle name (or leave empty to finish)")
            .allow_empty(true)
            .interact_text()
            .context("bundle name prompt failed")?;

        let name = if name.is_empty() {
            if bundles.is_empty() {
                "base".to_string()
            } else {
                break;
            }
        } else {
            if crate::paths::is_unsafe_join_target(&name) {
                eprintln!(
                    "Invalid bundle name '{name}'. Use letters, numbers, hyphens, underscores."
                );
                continue;
            }
            name
        };

        let bundle_dir = config_dir.join("bundles").join(&name);
        std::fs::create_dir_all(&bundle_dir)
            .with_context(|| format!("creating bundle dir {}", bundle_dir.display()))?;

        std::fs::create_dir_all(bundle_dir.join("skills"))
            .with_context(|| format!("creating skills dir in {}", bundle_dir.display()))?;
        std::fs::create_dir_all(bundle_dir.join("hooks"))
            .with_context(|| format!("creating hooks dir in {}", bundle_dir.display()))?;

        bundles.push(name);
    }

    Ok(bundles)
}

/// Prompt the user for a GitHub repo URL (or empty to skip).
fn prompt_github_repo(config_dir: &Path) -> Result<Option<String>> {
    use dialoguer::{Input, Select};

    let wants_repo = Select::new()
        .with_prompt("Store configuration in a GitHub repo?")
        .items([
            "Yes, I have a repo URL",
            "Yes, help me create one",
            "Skip for now",
        ])
        .default(2)
        .interact()
        .context("repo prompt failed")?;

    match wants_repo {
        0 => {
            let repo: String = Input::new()
                .with_prompt("GitHub repo URL (e.g. https://github.com/you/llmenv-config)")
                .interact_text()
                .context("repo URL prompt failed")?;
            Ok(Some(repo))
        }
        1 => {
            let owner: String = Input::new()
                .with_prompt("GitHub username or org")
                .validate_with(|input: &String| -> Result<(), &str> {
                    if input.contains('/') || input.contains(char::is_whitespace) {
                        Err("Invalid GitHub username or org")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()
                .context("GitHub owner prompt failed")?;
            let name: String = Input::new()
                .with_prompt("Repo name (e.g. llmenv-config)")
                .validate_with(|input: &String| -> Result<(), &str> {
                    if input.contains('/') || input.contains(char::is_whitespace) {
                        Err("Invalid repo name")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()
                .context("repo name prompt failed")?;

            let repo_url = format!("https://github.com/{owner}/{name}");
            eprintln!();
            eprintln!("To create the repo, run:");
            eprintln!("  gh repo create {owner}/{name} --public --clone");
            eprintln!();
            eprintln!("Then clone it to {}:", config_dir.display());
            eprintln!("  git clone {repo_url}.git {}", config_dir.display());

            let proceed = Select::new()
                .with_prompt("Continue with setup?")
                .items(["Yes, I'll set up the repo later", "Cancel setup"])
                .default(0)
                .interact()
                .context("continue prompt failed")?;

            if proceed == 0 {
                Ok(Some(repo_url))
            } else {
                anyhow::bail!("Setup cancelled by user");
            }
        }
        _ => Ok(None),
    }
}

/// Write the initial config.yaml file.
fn write_config(
    config_path: &Path,
    bundles: &[String],
    repo: Option<&str>,
    user_name: &str,
) -> Result<()> {
    // Load template as base
    let template = crate::config::generate_template();
    let mut config: Config = serde_yaml::from_str(&template)
        .context("parsing config template — template may be out of sync with Config struct")?;

    // Set up bundles
    config.bundle = bundles
        .iter()
        .map(|name| crate::config::Bundle {
            name: name.clone(),
            when: vec![user_name.to_string()],
        })
        .collect();

    // If repo was provided, set up the marketplace
    if let Some(repo_url) = repo {
        config.marketplace.push(crate::config::Marketplace {
            name: "my-config".to_string(),
            source: repo_url.to_string(),
        });
    }

    let yaml = serde_yaml::to_string(&config).context("serializing config")?;
    paths::write_owner_only_atomic(config_path, yaml.as_bytes())
        .with_context(|| format!("writing config {}", config_path.display()))?;

    Ok(())
}

/// Write AGENTS.md orientation file.
fn write_agents_md(agents_path: &Path, bundles: &[String]) -> Result<()> {
    let bundle_list: String = bundles
        .iter()
        .map(|b| format!("- **bundles/{b}/** — {b} bundle"))
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        r#"# Agent Orientation

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

## Bundles

{bundle_list}

---

Generated by `llmenv setup`. Edit freely — see https://phaedrus1992.github.io/llmenv/ for docs.
"#
    );

    paths::write_owner_only(agents_path, content.as_bytes())
        .with_context(|| format!("writing AGENTS.md to {}", agents_path.display()))?;
    Ok(())
}

/// Run the interactive setup wizard.
pub(super) fn run_setup(path: Option<PathBuf>, repo: Option<String>) -> Result<()> {
    let config_dir: PathBuf = match path {
        Some(p) => {
            let path_str = p
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("setup path is not valid UTF-8: {}", p.display()))?;
            PathBuf::from(paths::expand_tilde(path_str))
        }
        None => paths::config_dir()?,
    };

    if config_dir.join("config.yaml").exists() {
        use dialoguer::Select;
        let overwrite = Select::new()
            .with_prompt(format!(
                "Config already exists at {}. Overwrite?",
                config_dir.display()
            ))
            .items(["No, keep existing config", "Yes, re-run setup"])
            .default(0)
            .interact()
            .context("overwrite prompt failed")?;
        if overwrite == 0 {
            eprintln!("Keeping existing config. Run `llmenv setup` again to reconfigure.");
            return Ok(());
        }
    }

    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating config dir {}", config_dir.display()))?;

    // --- Phase 1: Scan existing configs ---
    eprintln!();
    eprintln!("🔍 Scanning for existing tool configurations...");
    let detected = scan_existing_configs();

    if detected.is_empty() {
        eprintln!("  No existing tool configs found. Starting fresh.");
    } else {
        eprintln!("  Found {} existing config(s):", detected.len());
        for dc in &detected {
            eprintln!("    • {} ({})", dc.source, dc.path.display());
        }
    }

    // --- Phase 2: GitHub repo ---
    let repo_url = if let Some(given) = repo {
        Some(given)
    } else {
        prompt_github_repo(&config_dir)?
    };

    // --- Phase 3: User identity ---
    use dialoguer::Input;
    let user_name: String = Input::new()
        .with_prompt("Your username (used for bundle tag matching)")
        .default(std::env::var("USER").unwrap_or_else(|_| "me".to_string()))
        .validate_with(|input: &String| -> Result<(), &str> {
            if input.contains(char::is_whitespace) {
                Err("Username must not contain spaces")
            } else if input.is_empty() {
                Err("Username must not be empty")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .context("username prompt failed")?;

    // --- Phase 4: Bundle setup ---
    let bundles = prompt_bundles(&config_dir)?;

    // --- Phase 5: Write config ---
    let config_path = config_dir.join("config.yaml");
    write_config(&config_path, &bundles, repo_url.as_deref(), &user_name)?;
    eprintln!("✓ Written config to {}", config_path.display());

    // Validate
    Config::load(&config_path)
        .with_context(|| format!("validating new config at {}", config_path.display()))?;
    eprintln!("✓ Config validated successfully");

    // --- Phase 6: AGENTS.md ---
    let agents_path = config_dir.join("AGENTS.md");
    if !agents_path.exists() {
        write_agents_md(&agents_path, &bundles)?;
        eprintln!("✓ Created {}", agents_path.display());
    }

    eprintln!();
    eprintln!("✅ llmenv setup complete!");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!(
        "  1. Edit {} to fine-tune your configuration",
        config_path.display()
    );
    eprintln!(
        "  2. Add agent instructions to your bundles (e.g. bundles/{}/CLAUDE.md)",
        bundles.first().map(|s| s.as_str()).unwrap_or("base")
    );
    eprintln!("  3. Run `llmenv regenerate` to materialize the config");
    if repo_url.is_some() {
        eprintln!(
            "  4. Push your config to the repo: cd {} && git init && git add . && git commit -m 'initial config' && git remote add origin <url> && git push",
            config_dir.display()
        );
    }

    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_existing_configs_does_not_panic() {
        // Should not panic when HOME is unset
        let _configs = scan_existing_configs();
        // Just check it returns without panic — result is environment-dependent
    }

    #[test]
    fn test_write_config_creates_valid_yaml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.yaml");
        let bundles = vec!["base".to_string(), "work".to_string()];

        write_config(&config_path, &bundles, None, "testuser").expect("write_config");
        assert!(config_path.is_file(), "config.yaml should exist");

        // Verify it parses as valid Config
        let loaded: Config = Config::load(&config_path).expect("should load valid config");
        assert_eq!(loaded.bundle.len(), 2);
    }

    #[test]
    fn test_write_config_with_repo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.yaml");

        write_config(
            &config_path,
            &["base".to_string()],
            Some("https://github.com/user/repo"),
            "testuser",
        )
        .expect("write_config with repo");

        let loaded: Config = Config::load(&config_path).expect("should load");

        // Verify marketplace was set
        assert!(
            !loaded.marketplace.is_empty(),
            "marketplace should be set when repo is given"
        );
    }

    #[test]
    fn test_write_agents_md() {
        let dir = tempfile::tempdir().expect("temp dir");
        let agents_path = dir.path().join("AGENTS.md");
        let bundles = vec!["base".to_string()];

        write_agents_md(&agents_path, &bundles).expect("write_agents_md");
        assert!(agents_path.is_file(), "AGENTS.md should exist");

        let content = fs::read_to_string(&agents_path).expect("read AGENTS.md");
        assert!(content.contains("Agent Orientation"));
        assert!(content.contains("bundles/base/"));
    }
}
