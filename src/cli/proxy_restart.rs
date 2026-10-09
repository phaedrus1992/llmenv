//! `llmenv doctor --restart-memory-proxy` (#2417).

use std::time::Duration;

use crate::cli::{ProxyStart, ensure_local_memory_proxy};
use crate::mcp::proxy_ops::{SpawnSource, StopOutcome, stop_proxy};
use crate::{paths, scope};

/// How long the proxy has to exit after SIGTERM. A proxy with a request in flight can take a
/// moment to finish it; five seconds matches the bind wait.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Stop the proxy that the pidfile names, then start it again.
pub(super) fn run(use_color: bool) -> anyhow::Result<()> {
    let pass = super::doctor_pass(use_color);
    let info = super::doctor_info(use_color);
    let config = crate::hook_run::load_cached_config(&paths::config_path()?)?;
    let config_dir = paths::config_dir()?;
    let active = scope::evaluate(&config, &scope::matcher::Env::detect_for_config(&config)?);
    let pid_path = crate::mcp::proxy::default_pid_path()?;
    match stop_proxy(&pid_path, STOP_WAIT)? {
        StopOutcome::StillRunning => {
            anyhow::bail!(
                "mcp-proxy did not exit within {} seconds of SIGTERM. Stop it by hand: find its \
                 pid in {}, check that `ps -o command= -p <pid>` shows `mcp-proxy`, then run \
                 `kill -9 <pid>`.",
                STOP_WAIT.as_secs(),
                pid_path.display()
            );
        }
        StopOutcome::NotProxy(command) => {
            anyhow::bail!(
                "the pid in {} runs `{}`, not mcp-proxy, so llmenv did not signal it. Delete the \
                 pidfile and run this command again.",
                pid_path.display(),
                crate::util::display_safe(&command)
            );
        }
        StopOutcome::Stopped => eprintln!("{pass} Stopped the memory proxy"),
        StopOutcome::Gone => eprintln!("{info} The memory proxy in the pidfile was not running"),
        StopOutcome::NoPidfile => eprintln!("{info} No memory proxy pidfile"),
    }
    match ensure_local_memory_proxy(&config, &config_dir, &active, SpawnSource::Restart) {
        ProxyStart::Started => eprintln!("{pass} Started the memory proxy"),
        ProxyStart::AlreadyRunning => {
            anyhow::bail!(
                "something that llmenv does not track already holds the memory proxy address, \
                 so no new proxy started. Find it with `lsof -iTCP:<port> -sTCP:LISTEN`, stop \
                 it, and run this command again."
            );
        }
        ProxyStart::NotLocal => eprintln!("{info} This host does not serve memory"),
        ProxyStart::Failed(cause) => anyhow::bail!("cannot start the memory proxy: {cause}"),
    }
    Ok(())
}
