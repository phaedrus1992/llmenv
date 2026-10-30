use std::path::PathBuf;

use llmenv::session_log::{FileLogLayer, FileSink, resolve_file_path};
use tracing_subscriber::{EnvFilter, prelude::*};

/// Resolve the session-log file sink's path. `None` turns the file sink off: a relative
/// fallback would write the log into the working directory, which is a repository in a hook.
fn session_log_file_path(configured: Option<&str>) -> Option<PathBuf> {
    resolve_file_path(configured)
        .inspect_err(|e| {
            eprintln!("llmenv: failed to resolve the session log path: {e:#}; the file log is off");
        })
        .ok()
}

/// `--version`/`-V` alone (clap's built-in version flag) never touches
/// session-log config or any hook, so loading `config.yaml` for it is pure
/// overhead on an otherwise config-free path.
fn wants_version_only() -> bool {
    // Reads argv only to detect a version flag. No argument reaches a shell or a path.
    // nosemgrep: rust.lang.security.args-os.args-os
    is_version_only_args(std::env::args_os().skip(1))
}

/// Testable core of [`wants_version_only`]: true iff the arg list (excluding
/// the binary name) is exactly one of `--version` or `-V`.
fn is_version_only_args(args: impl Iterator<Item = std::ffi::OsString>) -> bool {
    let mut args = args;
    matches!(args.next(), Some(a) if a == "--version" || a == "-V") && args.next().is_none()
}

fn main() {
    llmenv::panic_hook::install();
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

    let file_layer = resolved
        .file
        .as_ref()
        .is_some_and(|f| f.enabled)
        .then(|| {
            let path = session_log_file_path(resolved.file_path())?;
            Some(
                FileLogLayer::new(FileSink::new(path))
                    .with_filter(stderr_filter(std::env::var("RUST_LOG").ok().as_deref())),
            )
        })
        .flatten();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter(std::env::var("RUST_LOG").ok().as_deref())),
        )
        .with(file_layer)
        .init();

    if let Err(e) = llmenv::cli::run() {
        eprintln!("llmenv: {e:#}");
        std::process::exit(1);
    }
}

/// The filter for the stderr layer. With `RUST_LOG` unset it passes WARN and ERROR, so a user
/// sees the warnings that report a degraded result (#3061). `RUST_LOG` still overrides it.
fn stderr_filter(rust_log: Option<&str>) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::WARN.into())
        .parse_lossy(rust_log.unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<std::ffi::OsString> {
        raw.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn stderr_filter_shows_warnings_when_rust_log_is_unset() {
        let level = stderr_filter(None).max_level_hint();
        assert_eq!(level, Some(tracing_subscriber::filter::LevelFilter::WARN));
    }

    #[test]
    fn stderr_filter_follows_rust_log_when_it_is_set() {
        let level = stderr_filter(Some("debug")).max_level_hint();
        assert_eq!(level, Some(tracing_subscriber::filter::LevelFilter::DEBUG));
    }

    #[test]
    fn version_only_true_for_long_flag_alone() {
        assert!(is_version_only_args(args(&["--version"]).into_iter()));
    }

    #[test]
    fn version_only_true_for_short_flag_alone() {
        assert!(is_version_only_args(args(&["-V"]).into_iter()));
    }

    #[test]
    fn version_only_false_with_trailing_args() {
        assert!(!is_version_only_args(
            args(&["--version", "extra"]).into_iter()
        ));
        assert!(!is_version_only_args(args(&["-V", "extra"]).into_iter()));
    }

    #[test]
    fn version_only_false_for_no_args() {
        assert!(!is_version_only_args(args(&[]).into_iter()));
    }

    #[test]
    fn version_only_false_for_other_args() {
        assert!(!is_version_only_args(args(&["setup"]).into_iter()));
        assert!(!is_version_only_args(args(&["--help"]).into_iter()));
    }
}
