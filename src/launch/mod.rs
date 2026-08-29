//! `llmenv launch <engine>`: resolve the environment the same way `export`
//! does, then spawn `engine` as a supervised child process with that
//! environment applied on top of the inherited one, inherited stdio, and the
//! child's exit code propagated as `launch`'s own (see #1056).
//!
//! Extracted from `crate::cli` (#1480) so the mid-session supervision work
//! (crash/restart, config-drift and credential-expiry notices) has its own
//! module rather than growing `cli`'s already-largest file further. See
//! `docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md`.

mod credential_watch;
mod drift;
mod icebreaker;
pub(crate) mod proxy;
pub(crate) mod sandbox;
pub(crate) mod socket;

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::sync::Arc;

use anyhow::Context;

const RELAUNCH_MAX_ATTEMPTS: usize = 3;
const RELAUNCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Caps how many times `launch` will relaunch a crashing child within a
/// rolling window, so a child that crashes on every start doesn't loop
/// forever. Attempts older than [`RELAUNCH_WINDOW`] no longer count.
#[derive(Debug, Default)]
struct RelaunchCap {
    attempts: Vec<std::time::Instant>,
}

impl RelaunchCap {
    /// Record an attempt at `now` and report whether the cap still allows
    /// relaunching (i.e. this attempt was the `RELAUNCH_MAX_ATTEMPTS`-th or
    /// earlier within the window).
    fn record_and_check(&mut self, now: std::time::Instant) -> bool {
        self.attempts
            .retain(|t| now.duration_since(*t) < RELAUNCH_WINDOW);
        self.attempts.push(now);
        self.attempts.len() <= RELAUNCH_MAX_ATTEMPTS
    }
}

/// Suffixes that mark an env var name as credential-shaped for the #1669
/// heuristic — a variable icebreaker doesn't seal but is still likely to
/// carry a live secret into the container as-is.
const CREDENTIAL_VAR_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_SECRET"];

/// The first key in `vars` that looks like it carries a credential (#1669),
/// by name only — never inspects the value. `None` when nothing matches.
fn first_credential_shaped_var(vars: &BTreeMap<String, String>) -> Option<&str> {
    vars.keys()
        .find(|key| {
            CREDENTIAL_VAR_SUFFIXES
                .iter()
                .any(|suffix| key.ends_with(suffix))
        })
        .map(String::as_str)
}

/// Appends `name: value` to `vars`'s `ANTHROPIC_CUSTOM_HEADERS` entry
/// (newline-separated, per Claude Code's own format for that variable)
/// rather than overwriting it — an existing value came from the user's own
/// config (e.g. a corporate gateway's tracking header) and must survive
/// alongside the launch proxy's own peer-auth header (#1632).
///
/// `name`/`value` must not themselves contain `\r`/`\n` — this crate's own
/// call site only ever passes a hardcoded header name and a hex-encoded
/// [`crate::launch::socket::LaunchToken`], neither of which can, so this
/// isn't validated here; it would be defending against an input this
/// function's only caller cannot produce.
fn append_custom_header(vars: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let line = format!("{name}: {value}");
    vars.entry("ANTHROPIC_CUSTOM_HEADERS".to_string())
        .and_modify(|existing| {
            existing.push('\n');
            existing.push_str(&line);
        })
        .or_insert(line);
}

/// The scope-narrowing flags `launch` shares with `export` (#1384), bundled so
/// [`run`] stays inside the 5-positional-param limit and so the three
/// always travel to [`crate::cli::resolve_env`] together.
pub(crate) struct LaunchScope {
    pub(crate) scope: Option<String>,
    pub(crate) tag: Option<String>,
    pub(crate) compress: bool,
    pub(crate) auto_restart: bool,
    /// `--container`/`--no-container` (#1080): overrides
    /// `features.sandbox.enabled` for this invocation. `None` means "use the
    /// config value".
    pub(crate) container_override: Option<bool>,
}

/// `llmenv launch <engine>`: resolve the environment the same way `export`
/// does, then spawn `engine` as a supervised child process with that
/// environment applied on top of the inherited one, inherited stdio, and the
/// child's exit code propagated as `launch`'s own (see #1056).
pub(crate) fn run(engine: &str, args: Vec<String>, narrow: LaunchScope) -> anyhow::Result<()> {
    let adapter = crate::adapter::adapter_for_launch_target(engine).ok_or_else(|| {
        anyhow::anyhow!(
            "unrecognized engine '{engine}' — expected one of: {}",
            crate::adapter::registered_adapters()
                .iter()
                .map(|a| a.binary_name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    // One resolution, used both as the "is it installed" gate and as the thing
    // actually spawned — see `command_for_binary`. Resolving PATH directly means
    // a negative result really is a missing engine, not an artifact of `which`
    // being unavailable (#1382).
    let Some(bin_path) = crate::paths::resolve_on_path(adapter.binary_name()) else {
        anyhow::bail!(
            "'{bin}' not found on PATH — install it before running `llmenv launch {engine}`",
            bin = adapter.binary_name()
        );
    };

    let mut resolved = crate::cli::resolve_env(narrow.scope, narrow.tag, narrow.compress)?;
    let config_path = crate::paths::config_path()?;
    let sandbox_spec = resolve_sandbox_spec(&config_path, narrow.container_override)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for launch")?;

    // `UnixListener::bind` (like `tokio::process::Command::spawn`) registers
    // with the runtime's reactor immediately, so it must run inside
    // `block_on` too — binding before entering the runtime panics with "no
    // reactor running", the same pitfall `run_supervised`'s doc comment
    // warns about for spawning the child.
    let status = rt.block_on(async {
        // The notice channel (drift/credential-expiry warnings) is a pure
        // add-on — a bind failure must not take down the actual engine
        // session, so it's logged and skipped rather than propagated.
        // `_cleanup` is bound here (not further down) so it stays alive
        // for the whole supervised session whenever a socket exists.
        let (socket_path, notices, token, _cleanup) = match socket::bind(std::process::id()) {
            Ok((listener, notices, path, token)) => {
                tokio::spawn(socket::serve(listener, Arc::clone(&notices), token.clone()));
                let cleanup = Some(SocketCleanup(path.clone()));
                (Some(path), Some(notices), Some(token), cleanup)
            }
            Err(e) => {
                eprintln!(
                    "llmenv: could not open launch's notice socket, continuing \
                         without config-drift/credential-expiry warnings: {e:#}"
                );
                (None, None, None, None)
            }
        };

        // icebreaker (#1651) only matters for a sandboxed launch — it exists
        // to keep the raw credential out of the container, so a host launch
        // has nothing for it to protect.
        let icebreaker_session = if let Some(spec) = &sandbox_spec {
            icebreaker::prepare(adapter.name(), spec.runtime, &resolved.vars).await?
        } else {
            None
        };
        let container_vars = icebreaker_session.as_ref().map(|s| &s.container_vars);

        // #1669: icebreaker only seals a credential for Claude Code — any
        // other engine still gets a raw credential-shaped var forwarded into
        // the container as-is (via the env-file, not argv, so it isn't
        // world-readable, but the container itself gets the real value with
        // no sealing). Warn so this doesn't silently undercut sandbox mode's
        // credential-containment pitch for a user who doesn't read the fine
        // print.
        if sandbox_spec.is_some()
            && icebreaker_session.is_none()
            && let Some(var) = first_credential_shaped_var(&resolved.vars)
        {
            eprintln!(
                "llmenv: sandbox mode is active and '{var}' looks like a credential, but \
                 icebreaker only seals credentials for Claude Code — {engine} will receive \
                 the raw value unsealed inside the container"
            );
        }

        // A container using icebreaker's sealed-token proxy has its outbound
        // traffic routed there directly (see `icebreaker.rs`'s module doc
        // comment) — launch_proxy's own rewrite rules would never see it,
        // silently no-op'd rather than applied. Reject the combination
        // explicitly instead of picking one winner behind the user's back.
        if icebreaker_session.is_some() {
            let launch_proxy_enabled = crate::config::Config::load(&config_path)
                .ok()
                .and_then(|c| c.features)
                .and_then(|f| f.launch_proxy)
                .is_some_and(|p| p.enabled);
            if launch_proxy_enabled {
                anyhow::bail!(
                    "features.sandbox and features.launch_proxy cannot both be enabled for the \
                     same Claude Code launch yet — icebreaker's sealed-token proxy already owns \
                     the container's outbound traffic, so launch_proxy's rewrite rules would \
                     never apply"
                );
            }
        }

        let proxy_shutdown_tx: Option<tokio::sync::watch::Sender<bool>> =
            match crate::config::Config::load(&config_path) {
                Ok(config) => {
                    // Claude Code only for now (#1289's approved design
                    // scope — `ANTHROPIC_BASE_URL` is Claude Code/Anthropic-
                    // SDK specific); same gating pattern as the credential-
                    // watch wiring below (`adapter.name() == "claude-code"`).
                    let launch_proxy = config
                        .features
                        .as_ref()
                        .and_then(|f| f.launch_proxy.as_ref())
                        .filter(|p| p.enabled && adapter.name() == "claude-code");
                    match launch_proxy {
                        Some(launch_proxy) => match proxy::bind().await {
                            Ok((listener, addr, proxy_token)) => {
                                let upstream_str = resolved
                                    .vars
                                    .get("ANTHROPIC_BASE_URL")
                                    .cloned()
                                    .unwrap_or_else(|| "https://api.anthropic.com".to_string());
                                match upstream_str.parse::<url::Url>() {
                                    Ok(upstream) => {
                                        let rules = Arc::new(launch_proxy.rules.clone());
                                        let (tx, rx) = tokio::sync::watch::channel(false);
                                        tokio::spawn(proxy::serve(
                                            listener,
                                            upstream,
                                            rules,
                                            proxy_token.clone(),
                                            rx,
                                        ));
                                        resolved.vars.insert(
                                            "ANTHROPIC_BASE_URL".to_string(),
                                            format!("http://{addr}"),
                                        );
                                        append_custom_header(
                                            &mut resolved.vars,
                                            proxy::PEER_AUTH_HEADER,
                                            proxy_token.as_str(),
                                        );
                                        Some(tx)
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "llmenv: could not parse existing ANTHROPIC_BASE_URL \
                                             '{upstream_str}', launch proxy disabled for this \
                                             session: {e}"
                                        );
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "llmenv: could not start launch proxy, continuing without \
                                     request rewriting: {e:#}"
                                );
                                None
                            }
                        },
                        None => None,
                    }
                }
                Err(e) => {
                    tracing::debug!("launch: could not load config, launch proxy disabled: {e:#}");
                    None
                }
            };

        if let Some(notices) = &notices {
            match drift::current_hash(&config_path) {
                Ok(Some(baseline)) => {
                    tokio::spawn(drift::watch(
                        baseline,
                        config_path.clone(),
                        Arc::clone(notices),
                        drift::DRIFT_CHECK_INTERVAL,
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!("launch: no drift baseline, drift watch disabled: {e:#}");
                }
            }
            if adapter.name() == "claude-code" {
                match crate::config::Config::load(&config_path) {
                    Ok(config) => {
                        let cache_dir = std::path::PathBuf::from(crate::paths::expand_tilde(
                            &config.cache.cache_dir,
                        ));
                        let adapter_root = cache_dir.join(adapter.name());
                        tokio::spawn(credential_watch::watch(
                            adapter_root,
                            Arc::clone(notices),
                            credential_watch::EXPIRY_CHECK_INTERVAL,
                        ));
                    }
                    Err(e) => {
                        tracing::debug!(
                            "launch: could not load config, credential-expiry watch disabled: {e:#}"
                        );
                    }
                }
            }
        }

        let notice_socket = match (&socket_path, &token) {
            (Some(path), Some(token)) => Some(NoticeSocket { path, token }),
            _ => None,
        };
        let result = supervision_loop(
            EngineTarget {
                adapter: adapter.as_ref(),
                bin_path: &bin_path,
                args: &args,
                sandbox: sandbox_spec.as_ref(),
                container_vars,
            },
            &resolved,
            notice_socket,
            narrow.auto_restart,
        )
        .await;
        if let Some(tx) = proxy_shutdown_tx {
            let _ = tx.send(true);
        }
        result
    })?;

    crate::cli::exit_with_status(status);
}

/// Unlinks the per-session socket on every exit path via `Drop`, including a
/// panic unwind — the socket is the one artifact `launch` genuinely owns for
/// its own lifetime (see design doc "Teardown").
struct SocketCleanup(std::path::PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("launch: could not remove socket {}: {e}", self.0.display());
        }
    }
}

/// The engine identity and invocation the supervision loop relaunches
/// unchanged on every crash — bundled so [`supervision_loop`] stays inside
/// the 5-positional-param limit.
struct EngineTarget<'a> {
    adapter: &'a dyn crate::adapter::AgentAdapter,
    bin_path: &'a std::path::Path,
    args: &'a [String],
    /// `Some` when this launch runs the engine in a container (#1080)
    /// instead of directly on the host.
    sandbox: Option<&'a sandbox::SandboxSpec>,
    /// Env vars to forward into the container in place of the plain resolved
    /// ones (#1651) — `Some` only once icebreaker has sealed a credential;
    /// falls back to the ordinary resolved vars otherwise. Unused when
    /// `sandbox` is `None`.
    container_vars: Option<&'a BTreeMap<String, String>>,
}

/// Decide whether `narrow`'s launch runs sandboxed, and if so, resolve the
/// concrete container runtime + image.
///
/// `override_enabled` is `--container`/`--no-container` (`None` means "use
/// `features.sandbox.enabled`"). A config load failure is tolerated the same
/// way [`run`]'s `launch_proxy` gating tolerates one — sandboxing is treated
/// as disabled rather than failing the launch — unless the override
/// explicitly forces sandboxing on, in which case there is no config to fall
/// back to and the launch must fail rather than silently run unsandboxed.
/// Unlike `launch_proxy` (a convenience feature), this downgrade is printed
/// to stderr rather than only logged at `debug!`: sandboxing is a containment
/// boundary, so silently falling back to an unsandboxed host launch needs to
/// be visible by default, not opt-in via a log filter the user doesn't know
/// to enable.
///
/// # Errors
/// Bails when sandboxing is active and either no configured container
/// runtime is found on `PATH`, or no image is configured — llmenv does not
/// yet publish a default sandbox image (#1653), so one must be set
/// explicitly via `features.sandbox.image` until it does.
fn resolve_sandbox_spec(
    config_path: &std::path::Path,
    override_enabled: Option<bool>,
) -> anyhow::Result<Option<sandbox::SandboxSpec>> {
    let sandbox_config = match crate::config::Config::load(config_path) {
        Ok(config) => config.features.and_then(|f| f.sandbox),
        Err(e) => {
            if override_enabled == Some(true) {
                return Err(e.context("--container requires a loadable config"));
            }
            eprintln!(
                "llmenv: could not load config, sandbox mode disabled for this launch: {e:#}"
            );
            None
        }
    };
    build_sandbox_spec(sandbox_config, override_enabled, sandbox::resolve_runtime)
}

/// [`resolve_sandbox_spec`]'s decision logic once the config has already been
/// loaded (or a load failure already handled). Split out, with the runtime
/// probe passed in as `resolve_runtime`, so this is unit-testable without
/// touching the filesystem or the process's real `PATH` — production always
/// passes [`sandbox::resolve_runtime`]; `sandbox::resolve_runtime`'s own tests
/// already cover the `PATH`-probing behavior itself.
fn build_sandbox_spec(
    sandbox_config: Option<llmenv_config::Sandbox>,
    override_enabled: Option<bool>,
    resolve_runtime: impl Fn(&llmenv_config::SandboxRuntime) -> Option<sandbox::ContainerRuntime>,
) -> anyhow::Result<Option<sandbox::SandboxSpec>> {
    let configured_enabled = sandbox_config.as_ref().is_some_and(|s| s.enabled);
    if !override_enabled.unwrap_or(configured_enabled) {
        return Ok(None);
    }
    let sandbox_config = sandbox_config.unwrap_or_default();
    let Some(runtime) = resolve_runtime(&sandbox_config.runtime) else {
        anyhow::bail!(
            "sandbox mode is enabled but none of [{}] were found on PATH",
            sandbox::requested_binaries(&sandbox_config.runtime)
        );
    };
    let Some(image) = sandbox_config.image else {
        anyhow::bail!(
            "sandbox mode is enabled but features.sandbox.image is unset — \
             llmenv does not yet publish a default sandbox image (#1653), so \
             an image must be configured explicitly"
        );
    };
    Ok(Some(sandbox::SandboxSpec {
        runtime,
        image,
        forward_ssh_agent: sandbox_config.forward_ssh_agent,
    }))
}

/// The mid-session notice socket's path and shared secret (#1484), bundled
/// since they always travel together — either both `Some` (the socket bound)
/// or both absent — and threading them separately would push
/// [`supervision_loop`] past its 5-positional-param limit.
struct NoticeSocket<'a> {
    path: &'a std::path::Path,
    token: &'a socket::LaunchToken,
}

/// Spawn the engine, relaunching it after a crash (up to the restart cap or
/// until the user declines), and return the final exit status once the
/// engine exits cleanly or the cap/decline path gives up.
async fn supervision_loop(
    target: EngineTarget<'_>,
    resolved: &crate::cli::ResolvedEnv,
    notice_socket: Option<NoticeSocket<'_>>,
    auto_restart: bool,
) -> anyhow::Result<std::process::ExitStatus> {
    let EngineTarget {
        adapter,
        bin_path,
        args,
        sandbox,
        container_vars,
    } = target;
    let mut cap = RelaunchCap::default();

    loop {
        // `_env_file_guard` must stay alive at least until `spawn_and_supervise`
        // returns — it deletes the sandbox env file on drop, and dropping it
        // before the container reads its env would race the file out from
        // under `docker`/`podman`.
        let (mut cmd, _env_file_guard) = match sandbox {
            Some(spec) => {
                let project_dir = std::env::current_dir()
                    .context("resolving the project directory to mount into the sandbox")?;
                // #1671: an explicit opt-out means the sandbox never sees the
                // host's SSH-agent socket, even when one is running — the
                // socket is a live signing oracle for that identity, not a
                // reduced-privilege view of it.
                let ssh_auth_sock = spec
                    .forward_ssh_agent
                    .then(|| std::env::var_os("SSH_AUTH_SOCK").map(std::path::PathBuf::from))
                    .flatten();
                let (cmd, guard) = sandbox::container_command(
                    spec,
                    sandbox::ContainerInputs {
                        binary_name: adapter.binary_name(),
                        args,
                        vars: container_vars.unwrap_or(&resolved.vars),
                        project_dir: &project_dir,
                        ssh_auth_sock: ssh_auth_sock.as_deref(),
                    },
                )?;
                (cmd, Some(guard))
            }
            None => {
                let mut cmd = crate::cli::command_at_path(bin_path, adapter.binary_name());
                cmd.args(args);
                for (key, value) in &resolved.vars {
                    cmd.env(key, value);
                }
                if let Some(ns) = &notice_socket {
                    cmd.env("LLMENV_LAUNCH_SOCKET", ns.path);
                    cmd.env("LLMENV_LAUNCH_TOKEN", ns.token.as_str());
                }
                (cmd, None)
            }
        };
        cmd.stdin(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());
        let mut cmd = tokio::process::Command::from(cmd);

        let spawned_binary_name = match sandbox {
            Some(spec) => spec.runtime.binary_name(),
            None => adapter.binary_name(),
        };
        let status = spawn_and_supervise(&mut cmd, spawned_binary_name, None).await?;

        if status.success() {
            return Ok(status);
        }

        let reason = match status.signal() {
            Some(sig) => format!("terminated by signal {sig}"),
            None => format!("exited with code {}", status.code().unwrap_or(-1)),
        };
        eprintln!("llmenv: engine {reason}");

        if !cap.record_and_check(std::time::Instant::now()) {
            eprintln!("llmenv: restart attempts exceeded, giving up");
            return Ok(status);
        }

        if auto_restart {
            eprintln!("llmenv: auto-restarting");
            continue;
        }

        eprint!("Restart? [y/N] ");
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0
            || !answer.trim().eq_ignore_ascii_case("y")
        {
            return Ok(status);
        }
    }
}

/// Spawn the engine and wait for it to exit, never dying on a signal itself —
/// `launch`'s exit status must always be the engine's, not a signal it happened
/// to receive first.
///
/// SIGINT and SIGTERM/SIGHUP are treated differently on purpose (#1383):
///
/// - **SIGINT is not forwarded.** The terminal generates it for the entire
///   foreground process group, so the engine already has its own copy, and an
///   agent TUI commonly reads a second interrupt as "force quit" — forwarding
///   would turn one Ctrl-C into two.
/// - **SIGTERM and SIGHUP are forwarded.** A terminal never generates SIGTERM,
///   so one that arrives here came from a supervisor targeting this process by
///   pid — `docker stop` signalling PID 1, systemd `KillMode=mixed`, a CI
///   runner doing `kill <pid>`. The engine would otherwise never learn it
///   should shut down, and nothing would exit until the caller's SIGKILL
///   deadline. Both signals mean "terminate", so the duplicate a rare
///   group-directed kill produces is harmless.
///
/// Either way `launch` keeps waiting afterwards rather than exiting, so the
/// engine gets to shut down and report its own status.
///
/// The handlers are installed *before* the spawn on purpose. Installing them
/// afterwards leaves a window in which a signal kills `launch` under its
/// default disposition while the engine it just started keeps running,
/// orphaning the child and returning a signal-derived status the caller has to
/// interpret as the engine's.
///
/// Unix-only, like `launch` itself — the shipped targets are linux-musl and
/// apple-darwin, and the whole design rests on process-group signal semantics.
pub(crate) async fn spawn_and_supervise(
    cmd: &mut tokio::process::Command,
    binary: &str,
    stdin_payload: Option<&[u8]>,
) -> anyhow::Result<std::process::ExitStatus> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sighup = signal(SignalKind::hangup()).context("failed to install SIGHUP handler")?;

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn '{binary}'"))?;

    // The write runs *inside* the select below rather than before it. Installing
    // the handlers above replaced their default disposition, so from that point
    // on SIGINT/SIGTERM/SIGHUP are only buffered until something calls `recv()` —
    // nothing does until the loop starts. Awaiting the write first meant that a
    // payload larger than the pipe buffer, sent to a child that wasn't draining
    // it, left llmenv blocked and killable by nothing but SIGKILL.
    let mut write = std::pin::pin!(write_stdin_payload(
        child.stdin.take(),
        stdin_payload,
        binary
    ));
    let mut writing = stdin_payload.is_some();

    loop {
        tokio::select! {
            status = child.wait() => {
                return status.context("failed to wait on child engine process");
            }
            result = &mut write, if writing => {
                writing = false;
                if let Err(e) = result {
                    // Deliberately not fatal. A failed write means the child
                    // closed its read end, so it has either exited already or
                    // decided not to read — and its own exit status and stderr
                    // explain that far better than an `EPIPE` here would. Keep
                    // waiting and let the `child.wait()` arm report the real
                    // outcome.
                    //
                    // Returning instead would also drop `child` without killing
                    // it, and a dropped `tokio::process::Child` keeps running —
                    // so the error path of the anti-orphaning fix would orphan the
                    // child. `error!`, not `warn!`: llmenv's default filter is
                    // ERROR-only, and a child that silently never received its
                    // input is not something to leave unexplained.
                    tracing::error!("could not send input to '{binary}': {e:#}");
                }
            }
            _ = sigint.recv() => {
                // Deliberately not forwarded — see the doc comment above.
                tracing::debug!("launch: received SIGINT, still waiting on child");
            }
            _ = sigterm.recv() => {
                forward_signal(&child, rustix::process::Signal::TERM, "SIGTERM");
            }
            _ = sighup.recv() => {
                forward_signal(&child, rustix::process::Signal::HUP, "SIGHUP");
            }
        }
    }
}

/// Write `payload` to `stdin` and close it, or do nothing when there's no
/// payload.
///
/// Takes the handle by value so the write can live in the supervision `select!`
/// without borrowing the `Child` the same `select!` is waiting on.
///
/// # Errors
/// Returns an error when a payload was requested but no stdin pipe was opened for
/// it, or when the write fails — including the `EPIPE` a child that exited before
/// reading produces.
async fn write_stdin_payload(
    stdin: Option<tokio::process::ChildStdin>,
    payload: Option<&[u8]>,
    binary: &str,
) -> anyhow::Result<()> {
    let Some(payload) = payload else {
        return Ok(());
    };
    let Some(stdin) = stdin else {
        anyhow::bail!("'{binary}' was spawned without a stdin pipe to write to");
    };
    write_child_stdin(stdin, payload, binary).await
}

/// Write `payload` to the child's stdin and close the pipe.
///
/// Closing it is the point: `setup`'s crush handoff feeds the skill text on
/// stdin, and crush reads until EOF — leaving the handle open would hang.
///
/// # Errors
/// Returns an error when the write or the close fails. A child that exited before
/// reading its input surfaces here as `EPIPE`, so the message says so rather than
/// reporting a bare "broken pipe".
async fn write_child_stdin(
    mut stdin: tokio::process::ChildStdin,
    payload: &[u8],
    binary: &str,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    stdin.write_all(payload).await.with_context(|| {
        format!("writing to '{binary}' stdin — it may have exited before reading its input")
    })?;
    // `shutdown` flushes and closes; the `stdin` handle is then dropped, so
    // nothing is left holding the write end open.
    stdin
        .shutdown()
        .await
        .with_context(|| format!("closing '{binary}' stdin"))
}

/// Send `signal` to the supervised engine, best-effort.
///
/// Goes through `rustix::process::kill_process` (a direct syscall) rather than
/// fork+exec'ing `kill`, mirroring `consolidation::kill_process_group`. Using
/// the `kill` binary here would reintroduce #1382's failure mode in exactly the
/// distroless-container case this forwarding exists to fix — no `kill` on the
/// image means no shutdown.
///
/// The pid can't have been recycled: `wait` has not completed (this runs from
/// the `select!` arm that races it), so the child is unreaped and its pid is at
/// worst a zombie's.
///
/// Failure is not propagated — the `child.wait()` arm is about to report the
/// engine's real status either way — but anything other than a lost race is
/// logged at `error!`. It has to be `error!` specifically: llmenv's default
/// `EnvFilter` is ERROR-only, so a `warn!` here would be invisible in exactly
/// the situation the user needs it (they ran `docker stop`, forwarding failed,
/// and the engine is still running with no explanation anywhere).
#[cfg(unix)]
fn forward_signal(child: &tokio::process::Child, signal: rustix::process::Signal, name: &str) {
    let Some(raw) = child.id() else {
        tracing::debug!("launch: {name} arrived after the engine exited; nothing to forward");
        return;
    };
    // The remaining guards are should-never-happen: tokio reports a real child
    // pid, which is positive and fits a pid_t. If one ever fires, pid handling
    // upstream is corrupt — a different class of problem from losing a race,
    // and worth surfacing rather than dropping.
    let Ok(raw) = i32::try_from(raw) else {
        tracing::error!("launch: engine pid {raw} does not fit in a pid_t; not forwarding {name}");
        return;
    };
    // Same rule as `consolidation::is_safe_kill_target`: a non-positive pid
    // would mean "my whole process group" or "every process I may signal".
    // `Pid::from_raw` only rejects 0, which this already excludes.
    if raw <= 1 {
        tracing::error!("launch: refusing to forward {name} to pid {raw}");
        return;
    }
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        tracing::error!("launch: engine pid {raw} is not a valid pid; not forwarding {name}");
        return;
    };
    match rustix::process::kill_process(pid, signal) {
        Ok(()) => tracing::debug!("launch: forwarded {name} to the engine"),
        // The engine exited between the pid check above and this syscall —
        // the same benign race as the `child.id()` arm, not worth alarming.
        Err(rustix::io::Errno::SRCH) => {
            tracing::debug!("launch: engine exited before {name} could be forwarded");
        }
        // EPERM here means something (a container security profile, a seccomp
        // filter) is blocking the signal outright, so every later forward will
        // fail too and the engine will never shut down on request.
        Err(e) => {
            tracing::error!(
                "launch: could not forward {name} to the engine: {e}. \
                 The engine may keep running until it is killed directly."
            );
        }
    }
}

/// Poll `notices` until something is queued or a generous timeout elapses.
/// A fixed sleep is flaky under CI load — a slow runner can miss even a
/// couple of ticks on a short interval, whereas polling only cares that the
/// notice eventually lands. Shared by `drift`'s and `credential_watch`'s
/// test modules rather than duplicated in both.
#[cfg(test)]
async fn wait_for_notice(notices: &socket::NoticeSlot) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if notices.lock().await.is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // #1669: first_credential_shaped_var
    #[test]
    fn first_credential_shaped_var_finds_an_api_key_suffix() {
        let mut vars = BTreeMap::new();
        vars.insert("OPENAI_API_KEY".to_string(), "sk-abc".to_string());
        assert_eq!(first_credential_shaped_var(&vars), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn first_credential_shaped_var_finds_a_token_suffix() {
        let mut vars = BTreeMap::new();
        vars.insert("GH_TOKEN".to_string(), "ghp_abc".to_string());
        assert_eq!(first_credential_shaped_var(&vars), Some("GH_TOKEN"));
    }

    #[test]
    fn first_credential_shaped_var_finds_a_secret_suffix() {
        let mut vars = BTreeMap::new();
        vars.insert("CLIENT_SECRET".to_string(), "shh".to_string());
        assert_eq!(first_credential_shaped_var(&vars), Some("CLIENT_SECRET"));
    }

    #[test]
    fn first_credential_shaped_var_returns_none_when_nothing_matches() {
        let mut vars = BTreeMap::new();
        vars.insert("PATH".to_string(), "/usr/bin".to_string());
        vars.insert("EDITOR".to_string(), "vim".to_string());
        assert_eq!(first_credential_shaped_var(&vars), None);
    }

    #[test]
    fn first_credential_shaped_var_returns_none_for_empty_vars() {
        assert_eq!(first_credential_shaped_var(&BTreeMap::new()), None);
    }

    proptest::proptest! {
        #[test]
        fn prop_first_credential_shaped_var_matches_only_configured_suffixes(
            prefix in "[A-Z_]{0,10}",
            suffix_idx in 0..CREDENTIAL_VAR_SUFFIXES.len(),
            value in ".{0,10}",
        ) {
            let key = format!("{prefix}{}", CREDENTIAL_VAR_SUFFIXES[suffix_idx]);
            let mut vars = BTreeMap::new();
            vars.insert(key.clone(), value);
            proptest::prop_assert_eq!(first_credential_shaped_var(&vars), Some(key.as_str()));
        }

        #[test]
        fn prop_first_credential_shaped_var_ignores_names_with_no_matching_suffix(
            key in "[A-Z_]{1,15}",
        ) {
            let has_suffix = CREDENTIAL_VAR_SUFFIXES.iter().any(|s| key.ends_with(s));
            proptest::prop_assume!(!has_suffix);
            let mut vars = BTreeMap::new();
            vars.insert(key, "value".to_string());
            proptest::prop_assert_eq!(first_credential_shaped_var(&vars), None);
        }
    }

    #[test]
    fn append_custom_header_inserts_a_fresh_entry_when_absent() {
        let mut vars = BTreeMap::new();
        append_custom_header(&mut vars, "x-llmenv-launch-proxy-token", "abc123");
        assert_eq!(
            vars.get("ANTHROPIC_CUSTOM_HEADERS").map(String::as_str),
            Some("x-llmenv-launch-proxy-token: abc123")
        );
    }

    #[test]
    fn append_custom_header_preserves_an_existing_value_newline_separated() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "ANTHROPIC_CUSTOM_HEADERS".to_string(),
            "X-Corp-Gateway-Id: gw-1".to_string(),
        );
        append_custom_header(&mut vars, "x-llmenv-launch-proxy-token", "abc123");
        assert_eq!(
            vars.get("ANTHROPIC_CUSTOM_HEADERS").map(String::as_str),
            Some("X-Corp-Gateway-Id: gw-1\nx-llmenv-launch-proxy-token: abc123")
        );
    }

    #[test]
    fn relaunch_cap_allows_up_to_the_configured_max_within_the_window() {
        let mut cap = RelaunchCap::default();
        let base = std::time::Instant::now();
        assert!(cap.record_and_check(base));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(1)));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(2)));
        // 4th attempt within the window exceeds the cap of 3.
        assert!(!cap.record_and_check(base + std::time::Duration::from_secs(3)));
    }

    #[test]
    fn relaunch_cap_resets_once_attempts_age_out_of_the_window() {
        let mut cap = RelaunchCap::default();
        let base = std::time::Instant::now();
        assert!(cap.record_and_check(base));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(1)));
        assert!(cap.record_and_check(base + std::time::Duration::from_secs(2)));
        assert!(!cap.record_and_check(base + std::time::Duration::from_secs(3)));
        // After RELAUNCH_WINDOW has passed since attempt 1, the earlier
        // attempts have aged out and no longer count against the cap.
        let long_after = base + RELAUNCH_WINDOW + std::time::Duration::from_secs(1);
        assert!(cap.record_and_check(long_after));
    }

    proptest::proptest! {
        /// `record_and_check`'s incremental retain-then-push must agree, at
        /// every step of an arbitrary sequence of attempts, with an
        /// independent full-history recount — not just the two fixed
        /// sequences above.
        #[test]
        fn relaunch_cap_matches_a_full_history_recount(
            deltas_ms in proptest::collection::vec(0u64..120_000, 1..20),
        ) {
            let mut cap = RelaunchCap::default();
            let base = std::time::Instant::now();
            let mut elapsed_ms: u64 = 0;
            let mut history: Vec<u64> = Vec::new();
            let window_ms = u64::try_from(RELAUNCH_WINDOW.as_millis()).unwrap_or(u64::MAX);

            for delta in deltas_ms {
                elapsed_ms += delta;
                history.push(elapsed_ms);
                let now = base + std::time::Duration::from_millis(elapsed_ms);
                let actual = cap.record_and_check(now);

                let count_in_window = history
                    .iter()
                    .filter(|&&t| elapsed_ms - t < window_ms)
                    .count();
                let expected = count_in_window <= RELAUNCH_MAX_ATTEMPTS;
                assert_eq!(actual, expected);
            }
        }

        /// #1632: `append_custom_header` must never overwrite a pre-existing
        /// `ANTHROPIC_CUSTOM_HEADERS` value — the appended result must always
        /// equal the original value plus a newline plus the new line, for an
        /// arbitrary existing value and an arbitrary name/value pair, not
        /// just the two hand-picked examples above.
        #[test]
        fn append_custom_header_preserves_arbitrary_existing_value(
            existing in proptest::option::of("[ -~]{0,40}"),
            name in "[-A-Za-z]{1,20}",
            value in "[ -~]{0,40}",
        ) {
            let mut vars = BTreeMap::new();
            if let Some(existing) = &existing {
                vars.insert("ANTHROPIC_CUSTOM_HEADERS".to_string(), existing.clone());
            }
            append_custom_header(&mut vars, &name, &value);

            let expected = match &existing {
                Some(existing) => format!("{existing}\n{name}: {value}"),
                None => format!("{name}: {value}"),
            };
            prop_assert_eq!(vars.get("ANTHROPIC_CUSTOM_HEADERS").cloned(), Some(expected));
        }
    }

    // #1649: build_sandbox_spec
    use llmenv_config::{Sandbox, SandboxRuntime};

    fn always_podman(_: &SandboxRuntime) -> Option<sandbox::ContainerRuntime> {
        Some(sandbox::ContainerRuntime::Podman)
    }

    fn never_found(_: &SandboxRuntime) -> Option<sandbox::ContainerRuntime> {
        None
    }

    #[test]
    fn build_sandbox_spec_returns_none_when_neither_config_nor_override_enable_it() {
        let result = build_sandbox_spec(None, None, always_podman).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn build_sandbox_spec_returns_none_when_override_forces_it_off() {
        let config = Sandbox {
            enabled: true,
            runtime: SandboxRuntime::Auto,
            image: Some("img".to_string()),
            ..Default::default()
        };
        let result = build_sandbox_spec(Some(config), Some(false), always_podman).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn build_sandbox_spec_uses_config_enabled_and_image_when_no_override() {
        let config = Sandbox {
            enabled: true,
            runtime: SandboxRuntime::Auto,
            image: Some("registry.example.com/img:latest".to_string()),
            ..Default::default()
        };
        let spec = build_sandbox_spec(Some(config), None, always_podman)
            .unwrap()
            .unwrap();
        assert_eq!(spec.runtime, sandbox::ContainerRuntime::Podman);
        assert_eq!(spec.image, "registry.example.com/img:latest");
        assert!(spec.forward_ssh_agent);
    }

    // #1671: forward_ssh_agent opt-out plumbs through to the resolved spec.
    #[test]
    fn build_sandbox_spec_carries_forward_ssh_agent_opt_out_from_config() {
        let config = Sandbox {
            enabled: true,
            runtime: SandboxRuntime::Auto,
            image: Some("img".to_string()),
            forward_ssh_agent: false,
        };
        let spec = build_sandbox_spec(Some(config), None, always_podman)
            .unwrap()
            .unwrap();
        assert!(!spec.forward_ssh_agent);
    }

    #[test]
    fn build_sandbox_spec_override_enables_it_even_when_config_is_absent_but_image_missing_fails() {
        let err = build_sandbox_spec(None, Some(true), always_podman).unwrap_err();
        assert!(err.to_string().contains("features.sandbox.image is unset"));
    }

    #[test]
    fn build_sandbox_spec_fails_when_no_runtime_is_found_on_path() {
        let config = Sandbox {
            enabled: true,
            runtime: SandboxRuntime::Docker,
            image: Some("img".to_string()),
            ..Default::default()
        };
        let err = build_sandbox_spec(Some(config), None, never_found).unwrap_err();
        assert!(err.to_string().contains("docker"));
    }

    proptest::proptest! {
        #[test]
        fn prop_build_sandbox_spec_decision_matches_override_precedence(
            configured_enabled in proptest::bool::ANY,
            override_enabled in proptest::option::of(proptest::bool::ANY),
            image_present in proptest::bool::ANY,
            runtime_found in proptest::bool::ANY,
        ) {
            let config = Sandbox {
                enabled: configured_enabled,
                runtime: SandboxRuntime::Auto,
                image: image_present.then(|| "img".to_string()),
                ..Default::default()
            };
            let resolver = move |_: &SandboxRuntime| {
                runtime_found.then_some(sandbox::ContainerRuntime::Podman)
            };
            let result = build_sandbox_spec(Some(config), override_enabled, resolver);

            let effective_enabled = override_enabled.unwrap_or(configured_enabled);
            if !effective_enabled {
                proptest::prop_assert!(matches!(result, Ok(None)));
            } else if !runtime_found || !image_present {
                proptest::prop_assert!(result.is_err());
            } else {
                proptest::prop_assert!(matches!(result, Ok(Some(_))));
            }
        }
    }
}
