use std::{fs::OpenOptions, sync::Mutex};
use tracing_subscriber::{EnvFilter, prelude::*};

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

fn main() {
    let session_log = llmenv_paths::config_path()
        .ok()
        .and_then(|p| llmenv_config::Config::load(&p).ok())
        .and_then(|c| c.session_log);

    let file_layer = session_log.and_then(|raw| {
        let path = expand_tilde(&raw);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|file| {
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(Mutex::new(file))
                    .with_filter(EnvFilter::from_default_env())
            })
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
