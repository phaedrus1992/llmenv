use std::path::PathBuf;

use llmenv::session_log::{FileLogLayer, FileSink, default_file_path};
use tracing_subscriber::{EnvFilter, prelude::*};

/// Resolve the session-log file sink's path: explicit `path:` override
/// (tilde-expanded) or `<state_dir>/session-log.jsonl`.
fn session_log_file_path(configured: Option<&str>) -> PathBuf {
    match configured {
        Some(raw) => PathBuf::from(llmenv_paths::expand_tilde(raw)),
        None => default_file_path().unwrap_or_else(|e| {
            eprintln!(
                "llmenv: failed to resolve default session log path: {e}; falling back to CWD"
            );
            PathBuf::from("session-log.jsonl")
        }),
    }
}

/// `--version`/`-V` alone (clap's built-in version flag) never touches
/// session-log config or any hook, so loading `config.yaml` for it is pure
/// overhead on an otherwise config-free path.
fn wants_version_only() -> bool {
    let mut args = std::env::args_os().skip(1);
    matches!(args.next(), Some(a) if a == "--version" || a == "-V") && args.next().is_none()
}

fn main() {
    // Resolved session-logging config (absent block → transcript on, file off).
    // Log config errors so they're visible even though we fall back to defaults
    // (tracing subscriber isn't initialized yet, so use eprintln!).
    let loaded_config = if wants_version_only() {
        None
    } else {
        let config_path = llmenv_paths::config_path();
        if let Err(ref e) = config_path {
            eprintln!("llmenv: failed to resolve config path: {e:#}");
        }
        config_path.ok().and_then(|p| {
            llmenv_config::Config::load(&p)
                .inspect_err(|e| {
                    eprintln!("llmenv: failed to load config from {}: {e:#}", p.display())
                })
                .ok()
        })
    };
    let resolved = loaded_config
        .as_ref()
        .map(|c| c.session_log_resolved())
        .unwrap_or_default();
    if let Some(config) = loaded_config {
        llmenv::hook_run::set_preloaded_config(config);
    }

    let file_layer = resolved.file.as_ref().is_some_and(|f| f.enabled).then(|| {
        let path = session_log_file_path(resolved.file_path());
        FileLogLayer::new(FileSink::new(path)).with_filter(EnvFilter::from_default_env())
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(EnvFilter::from_default_env()),
        )
        .with(file_layer)
        .init();

    if let Err(e) = llmenv::cli::run() {
        eprintln!("llmenv: {e:#}");
        std::process::exit(1);
    }
}
