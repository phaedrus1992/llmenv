//! The dispatch of the `llmenv` subcommands (#4055).
//!
//! One function holds one group of commands. A group function runs the commands that it knows and
//! hands any other command back, so no function holds the whole `match`.

use crate::cli::{
    ColorMode, Command, MemoryCommand, ReadOnceCommand, checked_checkpoint, clean, doctor, guard,
    prefill, proxy_restart, read_job_payload, run_completions, run_completions_install,
    run_context, run_edit, run_export, run_hook, run_init, run_login, run_plugin_sync,
    run_regenerate, run_statusline_cmd, run_sync, run_task_command, run_validate, setup, status,
    upgrade, warn_if_unknown_engine,
};
use crate::paths;

/// What a group function did with a command.
enum Routed {
    /// The group ran the command.
    Done,
    /// The group does not know the command.
    Pass(Command),
}
use Routed::{Done, Pass};

/// How a group function runs: it takes the command and the output settings.
type Group = fn(Command, bool, ColorMode) -> anyhow::Result<Routed>;

/// The groups, in the order that the dispatch tries them.
const GROUPS: [Group; 8] = [
    environment_commands,
    status_commands,
    shell_commands,
    deprecated_listing_commands,
    sync_commands,
    guard_commands,
    job_commands,
    data_commands,
];

/// Run `command`, or print the usage when there is none.
///
/// # Errors
/// Returns the error of the command that ran.
pub(super) fn dispatch(
    command: Option<Command>,
    use_color: bool,
    color: ColorMode,
) -> anyhow::Result<()> {
    let Some(mut command) = command else {
        eprintln!("Usage: llmenv [COMMAND]");
        eprintln!("Run 'llmenv --help' for more information.");
        return Ok(());
    };
    for group in GROUPS {
        match group(command, use_color, color)? {
            Done => return Ok(()),
            Pass(next) => command = next,
        }
    }
    // Every command belongs to a group. The `Command` enum has no other variant.
    anyhow::bail!("llmenv: the command has no handler. This is a bug. Run `llmenv --help`")
}

fn environment_commands(
    command: Command,
    use_color: bool,
    _color: ColorMode,
) -> anyhow::Result<Routed> {
    match command {
        Command::Doctor {
            restart_memory_proxy: true,
            ..
        } => proxy_restart::run(use_color)?,
        Command::Doctor {
            gc, all, probe_mcp, ..
        } => doctor::run_doctor(gc, all, probe_mcp, use_color)?,
        Command::Export {
            scope,
            tag,
            explain,
            compress,
        } => run_export(scope, tag, explain, compress)?,
        Command::Regenerate => run_regenerate()?,
        Command::Init { path, repo } => run_init(path, repo)?,
        Command::Setup(args) => setup::run_setup_command(args)?,
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

fn status_commands(command: Command, use_color: bool, _color: ColorMode) -> anyhow::Result<Routed> {
    match command {
        Command::Statusline => run_statusline_cmd(use_color)?,
        Command::Status { section } => status::run_status(section, use_color)?,
        Command::Context { bundle, why } => run_context(bundle.as_deref(), why, use_color)?,
        Command::Validate => run_validate(use_color)?,
        Command::Edit { bundle } => run_edit(bundle)?,
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

fn shell_commands(command: Command, _use_color: bool, _color: ColorMode) -> anyhow::Result<Routed> {
    match command {
        Command::Hook { shell } => run_hook(&shell)?,
        Command::Login { global } => run_login(global)?,
        Command::Completions {
            shell,
            install,
            dir,
            force,
        } => run_completions_command(shell, install, dir, force)?,
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

/// `llmenv completions`: print the script, or install it with `--install` or `--dir`.
fn run_completions_command(
    shell: Option<clap_complete::Shell>,
    install: bool,
    dir: Option<std::path::PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    if install || dir.is_some() {
        return run_completions_install(shell, dir, force);
    }
    let shell =
        shell.ok_or_else(|| anyhow::anyhow!("SHELL is required unless --install is passed"))?;
    run_completions(shell)
}

/// The old listing commands. Each one prints a deprecation line and runs `status <section>`.
fn deprecated_listing_commands(
    command: Command,
    use_color: bool,
    _color: ColorMode,
) -> anyhow::Result<Routed> {
    let (old, new, section) = match command {
        Command::ScopeLs => ("scope-ls", "scopes", status::StatusSection::Scopes),
        Command::TagLs => ("tag-ls", "tags", status::StatusSection::Tags),
        Command::BundleLs => ("bundle-ls", "bundles", status::StatusSection::Bundles),
        Command::McpLs => ("mcp-ls", "mcps", status::StatusSection::Mcps),
        Command::MarketplaceLs => (
            "marketplace-ls",
            "marketplaces",
            status::StatusSection::Marketplaces,
        ),
        Command::PluginLs => ("plugin-ls", "plugins", status::StatusSection::Plugins),
        other => return Ok(Pass(other)),
    };
    eprintln!("warning: '{old}' is deprecated; use 'status {new}' instead");
    status::run_status(Some(section), use_color)?;
    Ok(Done)
}

fn sync_commands(command: Command, use_color: bool, _color: ColorMode) -> anyhow::Result<Routed> {
    match command {
        Command::PluginSync => run_plugin_sync()?,
        Command::Sync { dry_run } => run_sync(dry_run)?,
        Command::Upgrade { check, track } => upgrade::run_upgrade(track, check)?,
        Command::Clean(args) => clean::run_clean(&args, use_color)?,
        Command::Prune(args) => clean::run_prune_alias(&args, use_color)?,
        Command::Prefill(args) => prefill::run_prefill(&args)?,
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

/// The commands that the hooks run on every tool call, outside `hook-run`.
fn guard_commands(command: Command, use_color: bool, _color: ColorMode) -> anyhow::Result<Routed> {
    match command {
        Command::CheckStale { auto_fix, engine } => {
            guard::dispatch_check_stale(use_color, auto_fix, &engine)?;
        }
        Command::ConfigContext { engine } => guard::dispatch_config_context(&engine)?,
        Command::ConfigGuard { engine } => guard::dispatch_config_guard(&engine),
        Command::TmpGuard { engine } => guard::dispatch_tmp_guard(&engine),
        Command::Throttle { event } => guard::dispatch_throttle(&event),
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

/// `hook-run` and the detached jobs that it starts.
fn job_commands(command: Command, _use_color: bool, _color: ColorMode) -> anyhow::Result<Routed> {
    match command {
        Command::HookRun { event, engine } => run_hook_run(&event, &engine)?,
        Command::SessionLogRecord { checkpoint } => {
            // The checkpoint check starts the notice spool, so a stdin error reaches the agent.
            let checkpoint = checked_checkpoint(checkpoint)?;
            let payload_json = read_job_payload()?;
            crate::session_log::detached::run_record(&payload_json, checkpoint.as_deref())?;
        }
        Command::IcmStore { checkpoint } => {
            let checkpoint = checked_checkpoint(checkpoint)?;
            let payload_json = read_job_payload()?;
            crate::hook_run::detached_store::run_icm_store(&payload_json, checkpoint.as_deref())?;
        }
        Command::StatuslinePrRefresh => crate::cli::statusline::widget::pr_refresh::run()?,
        Command::ConsolidationRun { checkpoint } => {
            let checkpoint = checked_checkpoint(checkpoint)?;
            crate::hook_run::detached_consolidation::run_consolidation(
                &paths::config_path()?,
                checkpoint.as_deref(),
            )?;
        }
        Command::CbmIndexRun { checkpoint } => {
            let checkpoint = checked_checkpoint(Some(checkpoint))?.unwrap_or_default();
            crate::hook_run::detached_cbm::run_cbm_index(&checkpoint)?;
        }
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

/// `llmenv hook-run <event>`: run the hook, and exit 2 when it blocks the tool call.
fn run_hook_run(event: &str, engine: &str) -> anyhow::Result<()> {
    tracing::debug!(engine, "hook-run invoked by engine");
    warn_if_unknown_engine(engine);
    if crate::hook_run::run(event, engine)? == crate::hook_run::HookExit::Block {
        // Exit 2 is what makes a `PreToolUse` deny stop the tool call on Claude Code, and the
        // plugin shim of opencode reads it too. Exiting here, and not returning an error, keeps
        // the deny reason (already on stderr and stdout) free of an `llmenv: ...` prefix.
        std::process::exit(2);
    }
    Ok(())
}

fn data_commands(command: Command, _use_color: bool, color: ColorMode) -> anyhow::Result<Routed> {
    match command {
        Command::ReadOnce { command } => run_read_once(command)?,
        Command::Memory { command } => run_memory(command)?,
        Command::Task { command } => run_task_command(*command, color)?,
        other => return Ok(Pass(other)),
    }
    Ok(Done)
}

fn run_read_once(command: ReadOnceCommand) -> anyhow::Result<()> {
    match command {
        ReadOnceCommand::Clear => guard::timed("read-once", || {
            crate::hook_run::read_once::clear_cache(&paths::state_dir()?)
        }),
    }
}

fn run_memory(command: MemoryCommand) -> anyhow::Result<()> {
    match command {
        MemoryCommand::Stats => crate::memory::stats(),
        MemoryCommand::List => crate::memory::list(),
        MemoryCommand::Diff => crate::memory::diff(),
        MemoryCommand::Prune { dry_run } => crate::memory::prune(dry_run),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    /// The number of subcommands that the groups above run. A new subcommand must join a group,
    /// and this number then changes. Without a group, the dispatch ends in an error at run time.
    const GROUPED_COMMANDS: usize = 39;

    #[test]
    fn every_subcommand_has_a_group() {
        let defined = crate::cli::Cli::command().get_subcommands().count();
        assert_eq!(
            defined, GROUPED_COMMANDS,
            "a subcommand was added or removed: put it in a group of dispatch.rs, then update \
             GROUPED_COMMANDS"
        );
    }
}
