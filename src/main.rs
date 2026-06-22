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
    let session_log = llmenv_paths::config_path()
        .ok()
        .and_then(|p| llmenv_config::Config::load(&p).ok())
        .and_then(|c| c.session_log);

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
