use std::path::PathBuf;

use llmenv::session_log::{FileLogLayer, FileSink, default_file_path};
use tracing_subscriber::{EnvFilter, prelude::*};

/// Resolve the session-log file sink's path: explicit `path:` override
/// (tilde-expanded) or `<state_dir>/session-log.jsonl`.
fn session_log_file_path(configured: Option<&str>) -> PathBuf {
    match configured {
        Some(raw) => PathBuf::from(llmenv_paths::expand_tilde(raw)),
        None => default_file_path().unwrap_or_else(|_| PathBuf::from("session-log.jsonl")),
    }
}

fn main() {
    // Resolved session-logging config (absent block → transcript on, file off).
    let resolved = llmenv_paths::config_path()
        .ok()
        .and_then(|p| llmenv_config::Config::load(&p).ok())
        .map(|c| c.session_log_resolved())
        .unwrap_or_default();

    let file_layer = resolved.file.then(|| {
        let path = session_log_file_path(resolved.path.as_deref());
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
