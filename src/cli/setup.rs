use crate::config::Config;
use crate::paths;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Read ~/.claude/settings.json if it exists.
fn read_claude_settings(home: &Path) -> Option<serde_json::Value> {
    let path = home.join(".claude").join("settings.json");
    if !path.is_file() {
        return None;
    }
    let bytes = std::fs::read(&path)
        .inspect_err(|e| eprintln!("llmenv: failed to read {}: {e:#}", path.display()))
        .ok()?;
    serde_json::from_slice(&bytes)
        .inspect_err(|e| eprintln!("llmenv: failed to parse {}: {e:#}", path.display()))
        .ok()
}

/// Read ~/.claude/plugins.json if it exists.
fn read_claude_plugins(home: &Path) -> Option<serde_json::Value> {
    let path = home.join(".claude").join("plugins.json");
    if !path.is_file() {
        return None;
    }
    let bytes = std::fs::read(&path)
        .inspect_err(|e| eprintln!("llmenv: failed to read {}: {e:#}", path.display()))
        .ok()?;
    serde_json::from_slice(&bytes)
        .inspect_err(|e| eprintln!("llmenv: failed to parse {}: {e:#}", path.display()))
        .ok()
}

/// Read ~/.claude/claude.md if it exists.
fn read_claude_md(home: &Path) -> Option<String> {
    let path = home.join(".claude").join("CLAUDE.md");
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(&path)
        .inspect_err(|e| eprintln!("llmenv: failed to read {}: {e:#}", path.display()))
        .ok()
}

/// Read ~/.claude/gemini.md if it exists.
fn read_gemini_md(home: &Path) -> Option<String> {
    let path = home.join(".claude").join("GEMINI.md");
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(&path)
        .inspect_err(|e| eprintln!("llmenv: failed to read {}: {e:#}", path.display()))
        .ok()
}

/// Read per-project settings from ~/.claude/projects/*/settings.json.
fn read_project_configs(home: &Path) -> BTreeMap<String, serde_json::Value> {
    let mut projects = BTreeMap::new();
    let projects_dir = home.join(".claude").join("projects");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else {
        return projects;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let settings_path = entry.path().join("settings.json");
        let Ok(bytes) = std::fs::read(&settings_path).inspect_err(|e| {
            eprintln!(
                "llmenv: failed to read project settings {}: {e:#}",
                settings_path.display()
            )
        }) else {
            continue;
        };
        let Ok(val) = serde_json::from_slice(&bytes).inspect_err(|e| {
            eprintln!(
                "llmenv: failed to parse project settings {}: {e:#}",
                settings_path.display()
            )
        }) else {
            continue;
        };
        projects.insert(name, val);
    }
    projects
}

/// Build the full enumeration JSON value.
fn build_enumeration(available: &[String], config_dir: &Path) -> serde_json::Value {
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    let claude_section = home.as_ref().map(|h| {
        let settings = read_claude_settings(h);
        let plugins = read_claude_plugins(h);
        let marketplaces = settings
            .as_ref()
            .and_then(|s| s.get("marketplaces").cloned())
            .or_else(|| {
                plugins
                    .as_ref()
                    .and_then(|p| p.get("marketplaces").cloned())
            });

        serde_json::json!({
            "settings": settings,
            "plugins": plugins,
            "marketplaces": marketplaces,
            "claude_md": read_claude_md(h),
            "gemini_md": read_gemini_md(h),
            "projects": read_project_configs(h),
        })
    });

    serde_json::json!({
        "version": 1,
        "user": user,
        "config_dir": config_dir.to_string_lossy(),
        "engines_available": available,
        "existing_configs": {
            "claude_code": claude_section
        },
        "created_bundles": ["base"]
    })
}

/// Write the enumeration JSON to {config_dir}/.llmenv-setup-state.json.
fn write_enumeration_json(config_dir: &Path, enumeration: &serde_json::Value) -> Result<()> {
    let path = config_dir.join(".llmenv-setup-state.json");
    let json = serde_json::to_string_pretty(enumeration).context("serializing enumeration JSON")?;
    paths::write_owner_only_atomic(&path, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The embedded setup skill content.
const SETUP_SKILL_SOURCE: &str = include_str!("../../skills/setup-llmenv/SKILL.md");

/// Write the setup skill to the bundle's skills directory.
fn install_setup_skill(config_dir: &Path) -> Result<PathBuf> {
    let skill_dir = config_dir
        .join("bundles")
        .join("base")
        .join("skills")
        .join("setup-llmenv");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating skill dir {}", skill_dir.display()))?;

    let skill_path = skill_dir.join("SKILL.md");
    let state_path = config_dir.join(".llmenv-setup-state.json");
    let content = SETUP_SKILL_SOURCE.replace("{STATE_PATH}", &state_path.to_string_lossy());
    paths::write_owner_only(&skill_path, content.as_bytes())
        .with_context(|| format!("writing skill {}", skill_path.display()))?;
    Ok(skill_path)
}

/// Present the engine handoff prompt and optionally launch the AI agent.
fn engine_handoff_prompt(
    skill_path: &Path,
    _state_path: &Path,
    available: &[String],
    no_launch: bool,
) -> Result<()> {
    if no_launch {
        return Ok(());
    }

    if available.is_empty() {
        eprintln!();
        eprintln!("No AI engines found on PATH. Install Claude Code or Crush to");
        eprintln!("run the interactive setup skill. The skill file is already");
        eprintln!("installed — invoke it manually when ready.");
        return Ok(());
    }

    use dialoguer::Select;
    let mut choices: Vec<String> = available
        .iter()
        .map(|e| match e.as_str() {
            "claude_code" => "Claude Code".to_string(),
            "crush" => "Crush".to_string(),
            other => other.to_string(),
        })
        .collect();
    choices.push("Skip (I'll run the skill later)".to_string());

    let selection = Select::new()
        .with_prompt("Launch the interactive setup skill?")
        .items(&choices)
        .default(0)
        .interact()
        .context("handoff prompt failed")?;

    if selection >= available.len() {
        eprintln!("You can run the skill later by invoking `setup-llmenv` in your AI agent.");
        return Ok(());
    }

    let engine_id = &available[selection];
    let skill_content = std::fs::read_to_string(skill_path).context("reading skill file")?;
    let state_path_str = _state_path.to_string_lossy();
    let skill_content = skill_content.replace("{STATE_PATH}", &state_path_str);

    eprintln!();
    eprintln!("🚀 Launching {engine_id} with the setup skill...");
    eprintln!("(The AI will guide you through the rest of the setup.)");
    eprintln!();

    match engine_id.as_str() {
        "claude_code" => {
            let status = std::process::Command::new("claude")
                .arg("-p")
                .arg(&skill_content)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
                .context("launching claude")?;
            if !status.success() {
                anyhow::bail!("claude exited with status {status}");
            }
        }
        "crush" => {
            use std::io::Write;
            let mut child = std::process::Command::new("crush")
                .arg("run")
                .arg("--quiet")
                .arg("Execute the setup-llmenv skill")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .context("launching crush")?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(skill_content.as_bytes())
                    .context("writing skill to crush stdin")?;
            }
            let status = child.wait().context("waiting for crush")?;
            if !status.success() {
                anyhow::bail!("crush exited with status {status}");
            }
        }
        other => anyhow::bail!("unsupported engine: {other}"),
    }

    eprintln!("✓ Setup complete!");
    Ok(())
}

/// Probe PATH for supported engines.
/// Returns adapter IDs of engines found (e.g. "claude_code", "crush").
fn probe_engines() -> Vec<String> {
    let mut found = Vec::new();
    let probes: &[(&str, &str)] = &[("claude", "claude_code"), ("crush", "crush")];
    for (binary, engine_id) in probes {
        if let Ok(out) = std::process::Command::new("which").arg(binary).output()
            && out.status.success()
        {
            found.push(engine_id.to_string());
        }
    }
    found
}

/// Compute which engines should be disabled given which are available.
fn compute_disabled_engines(available: &[String]) -> Vec<String> {
    const ALL_SUPPORTED: &[&str] = &["claude_code", "crush"];
    ALL_SUPPORTED
        .iter()
        .filter(|e| !available.contains(&e.to_string()))
        .map(|e| e.to_string())
        .collect()
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
    disabled_engines: &[String],
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

    // Set disabled engines based on which engines are (not) on PATH
    config.disabled_engines = disabled_engines.to_vec();

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

/// Re-scan existing configs and refresh the enumeration JSON + installed skill.
/// Does NOT modify config.yaml, AGENTS.md, or bundle contents.
fn run_rescan(config_dir: &Path, no_launch: bool) -> Result<()> {
    if !config_dir.join("config.yaml").is_file() {
        anyhow::bail!(
            "No existing config found at {}. Run `llmenv setup` first.",
            config_dir.display()
        );
    }

    // --- Phase 1: Scan existing configs ---
    eprintln!();
    eprintln!("🔍 Re-scanning for existing tool configurations...");
    let home_dir = std::env::var("HOME").ok().map(PathBuf::from);
    if let Some(ref h) = home_dir {
        let claude_settings = h.join(".claude").join("settings.json");
        if claude_settings.is_file() {
            eprintln!("  ✓ Found Claude Code settings");
        }
        let plugins = h.join(".claude").join("plugins.json");
        if plugins.is_file() {
            eprintln!("  ✓ Found Claude Code plugins");
        }
        let projects_dir = h.join(".claude").join("projects");
        if projects_dir.is_dir() {
            let count = std::fs::read_dir(&projects_dir)
                .map(|e| e.flatten().count())
                .unwrap_or(0);
            if count > 0 {
                eprintln!("  ✓ Found {count} Claude Code project config(s)");
            }
        }
    }

    // Probe engines
    let available = probe_engines();
    if available.is_empty() {
        eprintln!("  No supported AI engines found on PATH (claude, crush).");
    }

    // --- Phase 2: Refresh enumeration JSON ---
    let enumeration = build_enumeration(&available, config_dir);
    write_enumeration_json(config_dir, &enumeration)?;
    eprintln!("✓ Environment snapshot refreshed in .llmenv-setup-state.json");

    // Re-install the setup skill
    install_setup_skill(config_dir)?;
    eprintln!("✓ Setup skill re-installed to bundles/base/skills/setup-llmenv/");

    // --- Engine handoff ---
    let skill_path = config_dir
        .join("bundles")
        .join("base")
        .join("skills")
        .join("setup-llmenv")
        .join("SKILL.md");
    let state_path = config_dir.join(".llmenv-setup-state.json");
    engine_handoff_prompt(&skill_path, &state_path, &available, no_launch)?;

    eprintln!();
    eprintln!("✅ Rescan complete! Config files were not modified.");

    Ok(())
}

/// Run the interactive setup wizard.
pub(super) fn run_setup(
    path: Option<PathBuf>,
    repo: Option<String>,
    no_launch: bool,
    rescan: bool,
) -> Result<()> {
    let config_dir: PathBuf = match path {
        Some(p) => {
            let path_str = p
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("setup path is not valid UTF-8: {}", p.display()))?;
            PathBuf::from(paths::expand_tilde(path_str))
        }
        None => paths::config_dir()?,
    };

    if rescan {
        return run_rescan(&config_dir, no_launch);
    }

    if !no_launch && config_dir.join("config.yaml").exists() {
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
    let home_dir = std::env::var("HOME").ok().map(PathBuf::from);
    if let Some(ref h) = home_dir {
        let claude_settings = h.join(".claude").join("settings.json");
        if claude_settings.is_file() {
            eprintln!("  ✓ Found Claude Code settings");
        }
        let plugins = h.join(".claude").join("plugins.json");
        if plugins.is_file() {
            eprintln!("  ✓ Found Claude Code plugins");
        }
        let projects_dir = h.join(".claude").join("projects");
        if projects_dir.is_dir() {
            let count = std::fs::read_dir(&projects_dir)
                .map(|e| e.flatten().count())
                .unwrap_or(0);
            if count > 0 {
                eprintln!("  ✓ Found {count} Claude Code project config(s)");
            }
        }
    }

    // Probe engines and set up disabled_engines
    let available = probe_engines();
    if available.is_empty() {
        eprintln!("  No supported AI engines found on PATH (claude, crush).");
    }

    // --- Phase 2: Enumeration JSON ---
    let enumeration = build_enumeration(&available, &config_dir);
    write_enumeration_json(&config_dir, &enumeration)?;
    eprintln!("✓ Environment snapshot written to .llmenv-setup-state.json");

    // Install the setup skill into the bundle
    install_setup_skill(&config_dir)?;
    eprintln!("✓ Setup skill installed to bundles/base/skills/setup-llmenv/");

    // --- Phase 3: GitHub repo ---
    let repo_url = if let Some(given) = repo {
        Some(given)
    } else if !no_launch {
        prompt_github_repo(&config_dir)?
    } else {
        None
    };

    // --- Phase 4: User identity ---
    let user_name: String = if !no_launch {
        use dialoguer::Input;
        Input::new()
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
            .context("username prompt failed")?
    } else {
        std::env::var("USER").unwrap_or_else(|_| "me".to_string())
    };

    // --- Phase 5: Bundle setup ---
    let bundles = if !no_launch {
        prompt_bundles(&config_dir)?
    } else {
        // In non-interactive mode, create a default "base" bundle
        let base_dir = config_dir.join("bundles").join("base");
        std::fs::create_dir_all(&base_dir)
            .with_context(|| format!("creating bundle dir {}", base_dir.display()))?;
        std::fs::create_dir_all(base_dir.join("skills"))
            .with_context(|| format!("creating skills dir in {}", base_dir.display()))?;
        std::fs::create_dir_all(base_dir.join("hooks"))
            .with_context(|| format!("creating hooks dir in {}", base_dir.display()))?;
        vec!["base".to_string()]
    };

    // --- Phase 6: Write config ---
    let config_path = config_dir.join("config.yaml");
    let disabled = compute_disabled_engines(&available);
    write_config(
        &config_path,
        &bundles,
        repo_url.as_deref(),
        &user_name,
        &disabled,
    )?;
    eprintln!("✓ Written config to {}", config_path.display());

    // Validate
    Config::load(&config_path)
        .with_context(|| format!("validating new config at {}", config_path.display()))?;
    eprintln!("✓ Config validated successfully");

    // --- Phase 7: AGENTS.md ---
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

    // --- Phase 8: Engine handoff ---
    let skill_path = config_dir
        .join("bundles")
        .join("base")
        .join("skills")
        .join("setup-llmenv")
        .join("SKILL.md");
    let state_path = config_dir.join(".llmenv-setup-state.json");
    engine_handoff_prompt(&skill_path, &state_path, &available, no_launch)?;

    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_write_config_creates_valid_yaml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.yaml");
        let bundles = vec!["base".to_string(), "work".to_string()];

        write_config(&config_path, &bundles, None, "testuser", &[]).expect("write_config");
        assert!(config_path.is_file(), "config.yaml should exist");

        // Verify it parses as valid Config
        let loaded: Config = Config::load(&config_path).expect("should load valid config");
        assert_eq!(loaded.bundle.len(), 2);
    }

    #[test]
    fn test_compute_disabled_engines_none_available() {
        let disabled = compute_disabled_engines(&[]);
        assert_eq!(disabled.len(), 2);
        assert!(disabled.contains(&"claude_code".to_string()));
        assert!(disabled.contains(&"crush".to_string()));
    }

    #[test]
    fn test_compute_disabled_engines_all_available() {
        let disabled = compute_disabled_engines(&["claude_code".to_string(), "crush".to_string()]);
        assert!(disabled.is_empty());
    }

    #[test]
    fn test_compute_disabled_engines_partial() {
        let disabled = compute_disabled_engines(&["claude_code".to_string()]);
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0], "crush");
    }

    #[test]
    fn test_disabled_engines_in_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bundles = vec!["base".to_string()];
        let config_path = dir.path().join("config.yaml");
        write_config(
            &config_path,
            &bundles,
            None,
            "testuser",
            &["crush".to_string()],
        )
        .expect("write_config");
        let config = Config::load(&config_path).expect("load");
        assert!(
            config.disabled_engines.contains(&"crush".to_string()),
            "crush should be disabled"
        );
    }

    #[test]
    fn test_probe_engines_does_not_panic() {
        let engines = probe_engines();
        for e in &engines {
            assert!(e == "claude_code" || e == "crush", "unexpected engine: {e}");
        }
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
            &[],
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

    #[test]
    fn test_read_claude_settings_file_not_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = read_claude_settings(dir.path());
        assert!(
            result.is_none(),
            "should be None when no settings file exists"
        );
    }

    #[test]
    fn test_write_enumeration_json_creates_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let enumeration = serde_json::json!({"version": 1});
        write_enumeration_json(dir.path(), &enumeration).expect("write enumeration");
        let path = dir.path().join(".llmenv-setup-state.json");
        assert!(path.is_file(), "enumeration file should exist");
        let content = std::fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["version"], 1);
    }

    #[test]
    fn test_build_enumeration_has_required_fields() {
        let dir = tempfile::tempdir().expect("temp dir");
        let enumeration = build_enumeration(&["claude_code".to_string()], dir.path());
        let obj = enumeration
            .as_object()
            .expect("enumeration must be an object");
        assert!(obj.contains_key("version"));
        assert!(obj.contains_key("engines_available"));
        assert!(obj.contains_key("existing_configs"));
        assert!(obj.contains_key("created_bundles"));
    }

    #[test]
    fn test_read_project_configs_empty_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let projects = read_project_configs(dir.path());
        assert!(projects.is_empty(), "no projects dir should return empty");
    }

    #[test]
    fn test_install_setup_skill_creates_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let skill_path = install_setup_skill(dir.path()).expect("install");
        assert!(skill_path.is_file(), "SKILL.md should exist");
        let content = std::fs::read_to_string(&skill_path).expect("read");
        assert!(
            content.contains("Setup llmenv"),
            "should contain skill content"
        );
        assert!(
            !content.contains("{STATE_PATH}"),
            "placeholder should be resolved"
        );
        assert!(
            content.contains(".llmenv-setup-state.json"),
            "should reference state file"
        );
    }

    #[test]
    fn test_installed_skill_has_frontmatter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let skill_path = install_setup_skill(dir.path()).expect("install");
        let content = std::fs::read_to_string(&skill_path).expect("read");
        assert!(content.starts_with("---"), "skill should have frontmatter");
    }

    #[test]
    fn test_setup_no_launch_creates_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = run_setup(Some(dir.path().to_path_buf()), None, true, false);
        assert!(result.is_ok(), "setup should succeed: {:?}", result.err());

        assert!(
            dir.path().join("config.yaml").is_file(),
            "config.yaml should exist"
        );
        assert!(
            dir.path().join("AGENTS.md").is_file(),
            "AGENTS.md should exist"
        );
        assert!(
            dir.path().join(".llmenv-setup-state.json").is_file(),
            ".llmenv-setup-state.json should exist"
        );
        assert!(
            dir.path()
                .join("bundles/base/skills/setup-llmenv/SKILL.md")
                .is_file(),
            "SKILL.md should exist"
        );
    }

    #[test]
    fn test_setup_no_launch_enumeration_is_valid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = run_setup(Some(dir.path().to_path_buf()), None, true, false);
        assert!(result.is_ok());

        let state_path = dir.path().join(".llmenv-setup-state.json");
        let content = std::fs::read_to_string(&state_path).expect("read state");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["version"], 1);
        assert!(
            parsed["config_dir"]
                .as_str()
                .unwrap_or("")
                .contains(dir.path().to_str().unwrap()),
            "config_dir should reference the setup dir"
        );
    }

    #[test]
    fn test_setup_no_launch_with_repo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = run_setup(
            Some(dir.path().to_path_buf()),
            Some("https://github.com/user/config".to_string()),
            true,
            false,
        );
        assert!(result.is_ok(), "setup with repo should succeed");

        let config = Config::load(&dir.path().join("config.yaml")).expect("load config");
        assert!(
            !config.marketplace.is_empty(),
            "marketplace should be set when repo is given"
        );
    }

    #[test]
    fn test_rescan_fails_without_existing_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = run_setup(Some(dir.path().to_path_buf()), None, true, true);
        assert!(
            result.is_err(),
            "rescan should fail without existing config"
        );
        let err = format!("{}", result.err().unwrap());
        assert!(
            err.contains("Run `llmenv setup` first"),
            "error should mention running setup first: {err}"
        );
    }

    #[test]
    fn test_rescan_refreshes_enumeration_and_does_not_overwrite() {
        let dir = tempfile::tempdir().expect("temp dir");

        // First, run a full setup
        run_setup(Some(dir.path().to_path_buf()), None, true, false)
            .expect("initial setup should succeed");

        let config_yaml = dir.path().join("config.yaml");
        let original_config = std::fs::read_to_string(&config_yaml).expect("read config");

        // Grab a timestamp of the state file so we can verify it was refreshed
        let state_path = dir.path().join(".llmenv-setup-state.json");
        // Modify the state file to prove rescan updates it
        std::fs::write(&state_path, r#"{"stale": true}"#).expect("write stale state");

        let agents_path = dir.path().join("AGENTS.md");
        let original_agents = std::fs::read_to_string(&agents_path).expect("read AGENTS.md");

        // Now run rescan
        let result = run_setup(Some(dir.path().to_path_buf()), None, true, true);
        assert!(result.is_ok(), "rescan should succeed: {:?}", result.err());

        // Config should be unchanged
        let config_after = std::fs::read_to_string(&config_yaml).expect("read config after");
        assert_eq!(
            config_after, original_config,
            "config.yaml should not be modified by rescan"
        );

        // AGENTS.md should be unchanged
        let agents_after = std::fs::read_to_string(&agents_path).expect("read AGENTS.md after");
        assert_eq!(
            agents_after, original_agents,
            "AGENTS.md should not be modified by rescan"
        );

        // State file should be refreshed (not stale anymore)
        let state_after = std::fs::read_to_string(&state_path).expect("read state after");
        let parsed: serde_json::Value =
            serde_json::from_str(&state_after).expect("valid JSON after rescan");
        assert_eq!(parsed["version"], 1, "state should be valid after rescan");
        assert_eq!(
            parsed.get("stale").and_then(|v| v.as_bool()),
            None,
            "stale marker should be gone"
        );
    }

    #[test]
    fn test_rescan_reinstalls_skill() {
        let dir = tempfile::tempdir().expect("temp dir");

        // Run initial setup
        run_setup(Some(dir.path().to_path_buf()), None, true, false).expect("initial setup");

        let skill_path = dir.path().join("bundles/base/skills/setup-llmenv/SKILL.md");

        // Modify the skill to prove it gets refreshed
        std::fs::write(&skill_path, "# Tampered skill").expect("write tampered skill");

        // Run rescan
        run_setup(Some(dir.path().to_path_buf()), None, true, true).expect("rescan should succeed");

        let skill_after = std::fs::read_to_string(&skill_path).expect("read skill after");
        assert_ne!(
            skill_after, "# Tampered skill",
            "skill should have been re-installed"
        );
        assert!(
            skill_after.contains("Setup llmenv"),
            "re-installed skill should have valid content"
        );
    }

    #[test]
    fn test_rescan_with_custom_path() {
        let dir = tempfile::tempdir().expect("temp dir");

        // Initial setup
        run_setup(Some(dir.path().to_path_buf()), None, true, false).expect("initial setup");

        // Rescan with same custom path
        let result = run_setup(Some(dir.path().to_path_buf()), None, true, true);
        assert!(
            result.is_ok(),
            "rescan with custom path: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_engine_handoff_no_launch_skips() {
        let result = engine_handoff_prompt(
            Path::new("/tmp/skill.md"),
            Path::new("/tmp/state.json"),
            &["claude_code".to_string()],
            true,
        );
        assert!(result.is_ok(), "no_launch should skip without error");
    }

    #[test]
    fn test_engine_handoff_no_engines_prints_message() {
        let result = engine_handoff_prompt(
            Path::new("/tmp/skill.md"),
            Path::new("/tmp/state.json"),
            &[],
            false,
        );
        assert!(result.is_ok(), "no engines should not be an error");
    }
}
