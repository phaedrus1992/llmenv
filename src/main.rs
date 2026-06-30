use std::{fs::OpenOptions, io::BufWriter, sync::Mutex};
use tracing_subscriber::{EnvFilter, prelude::*};

fn open_session_log(path: &str) -> Option<std::fs::File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("llmenv: session_log: cannot open {path:?}: {e}");
            None
        }
    }
}

fn main() {
    // Resolved session-logging config (absent block → transcript on, file off).
    // Task 9 replaces this stop-gap with the session_log module's sink wiring;
    // here we only keep the file path working so the build compiles.
    let resolved = llmenv_paths::config_path()
        .ok()
        .and_then(|p| llmenv_config::Config::load(&p).ok())
        .map(|c| c.session_log_resolved())
        .unwrap_or_default();
    let session_log = resolved.file.then(|| {
        resolved.path.clone().unwrap_or_else(|| {
            llmenv_paths::state_dir()
                .map(|d| d.join("session-log.jsonl").to_string_lossy().into_owned())
                .unwrap_or_else(|_| "session-log.jsonl".to_string())
        })
    });

    let file_layer = session_log.and_then(|raw| {
        let path = llmenv_paths::expand_tilde(&raw);
        open_session_log(&path).map(|file| {
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(Mutex::new(BufWriter::new(file)))
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
