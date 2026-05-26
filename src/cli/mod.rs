use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "llme", version, about = "Universal scope-aware environment for AI coding agents")]
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
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Doctor { gc }) => {
            run_doctor(gc)?;
        }
        None => {
            // Default: run doctor
            run_doctor(false)?;
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
            let skill_count = std::fs::read_dir(&skills_dir)
                .ok()
                .map(|entries| entries.count())
                .unwrap_or(0);
            eprintln!("✓ skills/ directory exists ({} items)", skill_count);
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
