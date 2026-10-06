//! `llmenv doctor --restart-memory-proxy` (#2417).

use std::path::Path;
use std::time::Duration;

use crate::cli::{ProxyStart, ensure_local_memory_proxy};
use crate::{paths, scope};
use llmenv_mcp::proxy_ops::{SpawnSource, StopOutcome, stop_proxy};

/// How long the proxy has to exit after SIGTERM. A proxy with a request in flight can take a
/// moment to finish it; five seconds matches the bind wait.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Stop the proxy that the pidfile names, then start it again.
pub(super) fn run(use_color: bool) -> anyhow::Result<()> {
    let pass = super::doctor_pass(use_color);
    let info = super::doctor_info(use_color);
    let config = crate::hook_run::load_cached_config(&paths::config_path()?)?;
    let config_dir = paths::config_dir()?;
    let active = scope::evaluate(&config, &scope::matcher::Env::detect_for_config(&config));
    let pid_path = crate::mcp::proxy::default_pid_path()?;
    emit(
        report_stop(stop_proxy(&pid_path, STOP_WAIT)?, &pid_path)?,
        &pass,
        &info,
    );
    let start = ensure_local_memory_proxy(&config, &config_dir, &active, SpawnSource::Restart);
    emit(report_start(start)?, &pass, &info);
    Ok(())
}

/// One line of progress for the user: a step that worked, or a step that had nothing to do.
#[derive(Debug, PartialEq, Eq)]
enum Note {
    Pass(&'static str),
    Info(&'static str),
}

fn emit(note: Note, pass: &str, info: &str) {
    match note {
        Note::Pass(text) => eprintln!("{pass} {text}"),
        Note::Info(text) => eprintln!("{info} {text}"),
    }
}

/// What the stop step tells the user. An outcome that blocks the restart is an error.
fn report_stop(outcome: StopOutcome, pid_path: &Path) -> anyhow::Result<Note> {
    match outcome {
        StopOutcome::StillRunning => anyhow::bail!(
            "mcp-proxy did not exit within {} seconds of SIGTERM. Stop it by hand: find its \
             pid in {}, check that `ps -o command= -p <pid>` shows `mcp-proxy`, then run \
             `kill -9 <pid>`.",
            STOP_WAIT.as_secs(),
            pid_path.display()
        ),
        StopOutcome::NotProxy(command) => anyhow::bail!(
            "the pid in {} runs `{}`, not mcp-proxy, so llmenv did not signal it. Delete the \
             pidfile and run this command again.",
            pid_path.display(),
            crate::util::display_safe(&command)
        ),
        StopOutcome::Stopped => Ok(Note::Pass("Stopped the memory proxy")),
        StopOutcome::Gone => Ok(Note::Info(
            "The memory proxy in the pidfile was not running",
        )),
        StopOutcome::NoPidfile => Ok(Note::Info("No memory proxy pidfile")),
    }
}

/// What the start step tells the user. A start that did not happen, and should have, is an error.
fn report_start(start: ProxyStart) -> anyhow::Result<Note> {
    match start {
        ProxyStart::Started => Ok(Note::Pass("Started the memory proxy")),
        ProxyStart::AlreadyRunning => anyhow::bail!(
            "something that llmenv does not track already holds the memory proxy address, \
             so no new proxy started. Find it with `lsof -iTCP:<port> -sTCP:LISTEN`, stop \
             it, and run this command again."
        ),
        ProxyStart::NotLocal => Ok(Note::Info("This host does not serve memory")),
        ProxyStart::Failed(cause) => anyhow::bail!("cannot start the memory proxy: {cause}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn stop(outcome: StopOutcome) -> anyhow::Result<Note> {
        report_stop(outcome, Path::new("/state/mcp-proxy.pid"))
    }

    #[test]
    fn a_stop_that_cleared_the_way_is_reported_as_progress() {
        assert_eq!(
            stop(StopOutcome::Stopped).unwrap(),
            Note::Pass("Stopped the memory proxy")
        );
        assert!(matches!(stop(StopOutcome::Gone).unwrap(), Note::Info(_)));
        assert!(matches!(
            stop(StopOutcome::NoPidfile).unwrap(),
            Note::Info(_)
        ));
    }

    #[test]
    fn a_proxy_that_ignores_sigterm_is_an_error_with_the_manual_fix() {
        let err = stop(StopOutcome::StillRunning).unwrap_err().to_string();
        assert!(err.contains("/state/mcp-proxy.pid"), "{err}");
        assert!(err.contains("kill -9"), "{err}");
    }

    #[test]
    fn a_pid_that_is_not_the_proxy_is_an_error_naming_the_command() {
        let err = stop(StopOutcome::NotProxy("vim notes.md".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("vim notes.md"), "{err}");
        assert!(err.contains("did not signal"), "{err}");
    }

    #[test]
    fn a_command_line_with_control_characters_is_cleaned_in_the_error() {
        let err = stop(StopOutcome::NotProxy("evil\u{1b}[2Jcmd".into()))
            .unwrap_err()
            .to_string();
        assert!(!err.contains('\u{1b}'), "{err:?}");
    }

    #[test]
    fn start_results_map_to_progress_or_an_error() {
        assert_eq!(
            report_start(ProxyStart::Started).unwrap(),
            Note::Pass("Started the memory proxy")
        );
        assert!(matches!(
            report_start(ProxyStart::NotLocal).unwrap(),
            Note::Info(_)
        ));
        let held = report_start(ProxyStart::AlreadyRunning)
            .unwrap_err()
            .to_string();
        assert!(held.contains("lsof"), "{held}");
        let failed = report_start(ProxyStart::Failed("no binary".into()))
            .unwrap_err()
            .to_string();
        assert!(failed.contains("no binary"), "{failed}");
    }
}
