//! Sandboxed `llmenv launch` (#1080): resolve which container engine to use,
//! and (later issues in this design) build and run the container itself. See
//! `docs/design/issue-1080-sidecar-container.md`.

use std::collections::BTreeMap;

use anyhow::Context;
use llmenv_config::SandboxRuntime;

/// The concrete container engine chosen for a sandboxed launch — distinct from
/// [`SandboxRuntime`], which also carries the `Auto` variant this type
/// resolves away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerRuntime {
    Docker,
    Podman,
}

impl ContainerRuntime {
    /// The binary name on `PATH` this engine spawns as.
    pub(crate) fn binary_name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}

/// [`resolve_runtime`] against an explicit `PATH` value.
///
/// Split out so the probe is testable without mutating the process
/// environment — `std::env::set_var` is `unsafe` as of Rust 2024 and this
/// workspace denies `unsafe_code`, mirroring `llmenv_paths::resolve_in_path_list`.
///
/// `Auto` probes for `podman` first, then `docker` (the design's stated
/// preference order — colima's Docker CLI shim satisfies the `docker` probe,
/// so it needs no separate branch). `Docker`/`Podman` probe for exactly that
/// one binary rather than skipping the check — a forced engine missing from
/// `PATH` must still be reported as missing, not silently accepted.
fn resolve_runtime_in_path_list(
    configured: &SandboxRuntime,
    path_var: &std::ffi::OsStr,
) -> Option<ContainerRuntime> {
    let found = |name: &str| llmenv_paths::resolve_in_path_list(name, path_var).is_some();
    match configured {
        SandboxRuntime::Docker => found("docker").then_some(ContainerRuntime::Docker),
        SandboxRuntime::Podman => found("podman").then_some(ContainerRuntime::Podman),
        SandboxRuntime::Auto => {
            if found("podman") {
                Some(ContainerRuntime::Podman)
            } else if found("docker") {
                Some(ContainerRuntime::Docker)
            } else {
                None
            }
        }
    }
}

/// Resolve `configured` against the process's real `PATH`. Returns `None`
/// when the configured (or, for `Auto`, either) engine isn't on `PATH`.
pub(crate) fn resolve_runtime(configured: &SandboxRuntime) -> Option<ContainerRuntime> {
    let path_var = std::env::var_os("PATH")?;
    resolve_runtime_in_path_list(configured, &path_var)
}

/// Names the binary/binaries [`resolve_runtime`] looked for, for use in a
/// "not found" error message.
pub(crate) fn requested_binaries(configured: &SandboxRuntime) -> &'static str {
    match configured {
        SandboxRuntime::Docker => "docker",
        SandboxRuntime::Podman => "podman",
        SandboxRuntime::Auto => "podman or docker",
    }
}

/// How long [`probe_image_pullable`] waits for a `manifest inspect` registry
/// round trip before giving up. Matches `attestation.rs`'s `GH_TIMEOUT` —
/// both are a bounded, best-effort network check that must not hang
/// `llmenv doctor`.
const IMAGE_PULLABLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The `image` argument [`probe_image_pullable`] passes to `manifest
/// inspect`. Docker's subcommand takes a bare image reference and queries
/// the registry directly; Podman's needs the `docker://` transport prefix to
/// inspect a remote reference instead of looking for an already-local
/// manifest list.
fn manifest_inspect_target(runtime: ContainerRuntime, image: &str) -> String {
    match runtime {
        ContainerRuntime::Docker => image.to_string(),
        ContainerRuntime::Podman => format!("docker://{image}"),
    }
}

/// Checks that `image` can be pulled through `runtime`, without actually
/// downloading it (`llmenv doctor`, #1654) — queries the registry's manifest
/// directly via `manifest inspect` rather than a real `pull`, which would
/// spend bandwidth and disk on every doctor run.
///
/// # Errors
/// Only when the `manifest inspect` invocation itself couldn't be run or
/// timed out (see [`IMAGE_PULLABLE_TIMEOUT`]) — a definitive "not pullable"
/// answer (bad reference, no registry auth, image missing) is `Ok(false)`,
/// not an error.
pub(crate) fn probe_image_pullable(runtime: ContainerRuntime, image: &str) -> anyhow::Result<bool> {
    let target = manifest_inspect_target(runtime, image);
    let mut cmd = std::process::Command::new(runtime.binary_name());
    cmd.args(["manifest", "inspect", &target]);
    let output = super::run_with_timeout(cmd, IMAGE_PULLABLE_TIMEOUT).with_context(|| {
        format!(
            "running `{} manifest inspect {target}`",
            runtime.binary_name()
        )
    })?;
    Ok(output.status.success())
}

/// The hostname a container uses to reach the host's loopback, from inside
/// its own network namespace — `127.0.0.1` inside the container is the
/// container itself, not the host. Docker requires
/// `--add-host=host.docker.internal:host-gateway` on the `run` invocation for
/// this to resolve on Linux ([`container_command`] adds it); recent rootless
/// Podman resolves `host.containers.internal` without an equivalent flag.
/// Shared by `icebreaker.rs` (credential proxy) and `config_mount.rs` (ICM
/// MCP endpoint, #1652) — both need the same host-to-container address.
pub(crate) fn gateway_host(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "host.docker.internal",
        ContainerRuntime::Podman => "host.containers.internal",
    }
}

/// Resolved container settings for a sandboxed launch — the concrete engine
/// and image to run, after `Auto`/override resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxSpec {
    pub(crate) runtime: ContainerRuntime,
    pub(crate) image: String,
    /// Mirrors `llmenv_config::Sandbox::forward_ssh_agent` (#1671) — whether
    /// the caller should forward the host's `SSH_AUTH_SOCK` into the
    /// container at all.
    pub(crate) forward_ssh_agent: bool,
}

/// In-container path the project tree is mounted at (#1650) — the
/// devcontainer convention the design doc calls out.
const WORKSPACE_PATH: &str = "/workspace";

/// In-container path the host's `SSH_AUTH_SOCK` is mounted at (#1650), when
/// present. Namespaced under `llmenv-` so it can't collide with a path the
/// image itself already uses.
const SSH_AUTH_SOCK_PATH: &str = "/run/llmenv-ssh-agent.sock";

/// Everything [`container_command`] needs besides the resolved
/// [`SandboxSpec`] — bundled so the function stays inside the
/// 5-positional-param limit.
pub(crate) struct ContainerInputs<'a> {
    /// Absolute host path to the resolved engine binary (#1653) —
    /// bind-mounted read-only at the identical in-container path and exec'd
    /// directly by that path, rather than baked into the image. Mirrors
    /// `config_dir`'s "mount at itself" pattern: the image only needs a
    /// compatible libc, not the engine on its own `PATH`.
    pub(crate) bin_path: &'a std::path::Path,
    pub(crate) args: &'a [String],
    /// The resolved/materialized env (#1650) — written to an owner-only
    /// `--env-file` rather than repeated `-e KEY=VALUE` flags, which would
    /// put every value (including a sealed credential) into this process's
    /// own argv — world-readable via `/proc/<pid>/cmdline` on Linux for the
    /// life of the `docker`/`podman` invocation, not just same-uid. This is
    /// deliberately not a live mount of `~/.config/llmenv` — the container
    /// gets the already-resolved result, not the source config tree.
    pub(crate) vars: &'a BTreeMap<String, String>,
    /// Host directory bind-mounted read-write at [`WORKSPACE_PATH`].
    pub(crate) project_dir: &'a std::path::Path,
    /// Host `SSH_AUTH_SOCK`, if the launching shell has one running. Mounted
    /// read-only at [`SSH_AUTH_SOCK_PATH`] so the container never holds the
    /// private key itself — though a running agent socket is still a full
    /// signing oracle for that identity, not a reduced-privilege view of it;
    /// `:ro` only protects the socket file from being replaced, not what
    /// reaching it grants.
    pub(crate) ssh_auth_sock: Option<&'a std::path::Path>,
    /// Materialized config directory (e.g. `CLAUDE_CONFIG_DIR`) to bind-mount
    /// read-only at the identical in-container path (#1652, generalized to
    /// every adapter in #1698), so MCP servers/skills/plugins/settings are
    /// visible to the containerized engine. `None` for an adapter with no
    /// `AgentAdapter::config_dir_mount`, or when no config directory was
    /// resolved.
    pub(crate) config_dir: Option<&'a std::path::Path>,
    /// Absolute host path to the adapter's MCP-config file within
    /// `config_dir` (e.g. `config_dir/.claude.json`) — the overlay-mount
    /// target for `patched_mcp_config`. `None` when `config_dir` is `None`.
    pub(crate) mcp_config_path: Option<&'a std::path::Path>,
    /// Host path to a patched copy of `mcp_config_path` (an MCP server's URL
    /// rewritten from loopback to the container gateway host, #1652,
    /// #1698), overlay-mounted read-only over the real file at
    /// `mcp_config_path` so the rewritten URL wins without touching the
    /// host's own file. `None` when no entry needed rewriting, or
    /// `config_dir` is `None`.
    pub(crate) patched_mcp_config: Option<&'a std::path::Path>,
    /// Absolute host path to a runtime-written credential file inside
    /// `config_dir` (Claude Code's `.credentials.json`, Codex's
    /// `auth.json`) that must be masked with a `/dev/null` overlay when the
    /// directory is mounted — llmenv never wrote it, so mounting the
    /// directory must not also expose a live credential (#1652 pre-pr-review
    /// P0, generalized in #1698). `None` when the adapter has no such file
    /// (Crush, opencode) or `config_dir` is `None`.
    pub(crate) credential_file: Option<&'a std::path::Path>,
}

/// An env file written for one `docker`/`podman` invocation, deleted on drop
/// once that invocation (and the container it started) has exited — mirrors
/// `SocketCleanup`'s pattern for the launch notice socket.
#[derive(Debug)]
pub(crate) struct EnvFileGuard(std::path::PathBuf);

impl Drop for EnvFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "launch: could not remove sandbox env file {}: {e}",
                self.0.display()
            );
        }
    }
}

/// Disambiguates concurrent [`write_env_file`] calls within the same process
/// — the relaunch loop can call it more than once per session, and several
/// tests call it within the same test binary (sharing one pid) in parallel.
static ENV_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Fallback staleness threshold for [`sweep_stale`]'s should-never-happen
/// case: a name that matches [`SANDBOX_TMP_PREFIX`] but not either known
/// filename shape, so no pid can be extracted to check liveness against.
/// Real entries are swept by liveness, not age (see [`sweep_stale`]'s doc
/// comment for why age alone is unsafe here) — a live sandboxed launch
/// routinely runs far longer than this.
const STALE_TEMP_FILE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// Runs [`sweep_stale`] at most once per process — every sandboxed launch
/// calls [`sandbox_tmp_dir`] at least once (via [`write_env_file`]), so this
/// still sweeps once per launch without re-scanning the directory on every
/// relaunch within the same session.
static SWEEP_DONE: std::sync::Once = std::sync::Once::new();

/// Owner-only subdirectory of llmenv's own state dir where sandbox launch
/// temp files (env files, patched config overlays) are written, replacing
/// the shared OS temp dir (#1705): a world-writable shared directory is a
/// worse home for files that can carry a sealed credential's headers or a
/// copy of `.claude.json` including any `mcpServers` auth headers/tokens,
/// even though each file is already written owner-only itself.
pub(crate) fn sandbox_tmp_dir() -> anyhow::Result<std::path::PathBuf> {
    prepare_sandbox_tmp_dir(&crate::paths::state_dir()?.join("sandbox-tmp"))
}

/// [`sandbox_tmp_dir`] against an explicit path — split out so the
/// symlink guard is testable without depending on the real state dir.
fn prepare_sandbox_tmp_dir(dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    // pre-pr-review P2 (#1705): `create_dir_owner_only`'s recursive
    // `DirBuilder` succeeds through a pre-existing symlink, and its
    // follow-up `set_permissions` chmods through one too — a symlink planted
    // here (same-uid write access to the state dir, outside this feature's
    // stated "different local user" threat model, but cheap to close) would
    // otherwise turn `sweep_stale` into an unfiltered reaper of whatever
    // directory it points at.
    anyhow::ensure!(
        !std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()),
        "{} is a symlink — refusing to use it as llmenv's sandbox temp dir",
        dir.display()
    );
    crate::paths::create_dir_owner_only(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    SWEEP_DONE.call_once(|| sweep_stale(dir, STALE_TEMP_FILE_AFTER));
    Ok(dir.to_path_buf())
}

/// Filename prefix every sandbox temp file is written with
/// ([`write_env_file`]'s `llmenv-sandbox-<pid>-<n>.env`,
/// `config_mount::write_patched_claude_json`'s
/// `llmenv-sandbox-claude-json-<pid>-<n>`) — [`sweep_stale`] never considers
/// an entry without it, so a symlink or stray file dropped into the same
/// directory by something else is never touched.
const SANDBOX_TMP_PREFIX: &str = "llmenv-sandbox-";

/// Extract the pid embedded in a sandbox temp file's name — the digits
/// between the last two `-` separators, ignoring a trailing `.env` (present
/// on [`write_env_file`]'s output, absent on
/// `config_mount::write_patched_claude_json`'s). `None` for anything that
/// doesn't match either shape.
fn pid_from_sandbox_tmp_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".env").unwrap_or(name);
    let (rest, _counter) = stem.rsplit_once('-')?;
    let (_, pid) = rest.rsplit_once('-')?;
    pid.parse().ok()
}

/// Whether `pid` still names a live process — `kill(pid, 0)`, which checks
/// existence without signaling it. A permission error still means the
/// process exists (just one this process can't signal); only "no such
/// process" means it's actually gone. Fails closed (reports alive) on an
/// unrepresentable pid or any other error, so a doubtful case is left for a
/// later sweep rather than reaped now.
fn process_is_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return true;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return true;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// Remove every entry directly inside `dir` that [`SANDBOX_TMP_PREFIX`]
/// identifies as llmenv's own and whose embedded pid is no longer running —
/// a crashed launch's `EnvFileGuard`/`PatchedFileGuard` never runs its
/// `Drop`, so its temp file would otherwise sit in `dir` forever.
///
/// Liveness, not age, decides staleness (pre-pr-review P1/P2, #1705): the
/// patched `.claude.json` overlay stays bind-mounted for a session's whole
/// lifetime, which routinely exceeds any fixed age threshold, so aging it
/// out would delete a live session's bind-mount source out from under it —
/// the next crash-relaunch would then fail with a missing mount source
/// instead of restarting. `stale_after` is only a fallback for the
/// should-never-happen case of a name matching the prefix but not either
/// known shape.
///
/// Best-effort: a scan, stat, or removal failure is logged and skipped
/// rather than failing the launch this sweep happens to run inside of.
fn sweep_stale(dir: &std::path::Path, stale_after: std::time::Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                "launch: could not scan {} for stale sandbox temp files: {e}",
                dir.display()
            );
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(
                    "launch: could not read a directory entry while sweeping {}: {e}",
                    dir.display()
                );
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue; // non-UTF8 name: llmenv never writes one, so not ours to touch.
        };
        if !name.starts_with(SANDBOX_TMP_PREFIX) {
            continue;
        }
        let is_stale = match pid_from_sandbox_tmp_name(&name) {
            Some(pid) => !process_is_alive(pid),
            None => {
                let stat = entry.metadata().and_then(|m| m.modified());
                let Ok(modified) = stat else {
                    tracing::warn!(
                        "launch: could not stat sandbox temp file {name} while sweeping"
                    );
                    continue;
                };
                std::time::SystemTime::now()
                    .duration_since(modified)
                    .is_ok_and(|age| age >= stale_after)
            }
        };
        if is_stale
            && let Err(e) = std::fs::remove_file(entry.path())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "launch: could not remove stale sandbox temp file {}: {e}",
                entry.path().display()
            );
        }
    }
}

/// Rejects a path that would break `--mount`'s comma-delimited `key=value`
/// option string: `,`/`=` have no escaping in that format, so either would
/// silently splice in an extra mount option or key rather than surviving as
/// part of the path (pre-pr-review P2, #1652/#1653).
fn validate_mount_path(path: &std::path::Path) -> anyhow::Result<()> {
    let s = path.display().to_string();
    anyhow::ensure!(
        !s.contains(',') && !s.contains('='),
        "path '{s}' contains ',' or '=', which --mount cannot represent safely"
    );
    Ok(())
}

/// Write `vars` to a fresh owner-only file in `--env-file` format (`KEY=VALUE`
/// per line, no quoting or escaping support — this is `docker`/`podman`'s own
/// format, not shell syntax).
///
/// # Errors
/// Rejects a value containing `\n`: `--env-file` has no escaping, so a literal
/// newline would silently splice in an extra line rather than surviving as
/// part of the value. Also bails on the underlying file write failing.
fn write_env_file(vars: &BTreeMap<String, String>) -> anyhow::Result<EnvFileGuard> {
    let mut content = String::new();
    for (key, value) in vars {
        anyhow::ensure!(
            !value.contains('\n'),
            "env var '{key}' contains a newline, which --env-file cannot represent safely"
        );
        content.push_str(key);
        content.push('=');
        content.push_str(value);
        content.push('\n');
    }
    let n = ENV_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = sandbox_tmp_dir()?.join(format!("llmenv-sandbox-{}-{n}.env", std::process::id()));
    crate::paths::write_owner_only(&path, content.as_bytes())
        .context("writing the sandbox env file")?;
    Ok(EnvFileGuard(path))
}

/// Build the container invocation for a sandboxed launch (#1080):
/// `<runtime> run --rm` plus the project/SSH/config-dir mounts, resolved env
/// (#1650), and baseline hardening flags, then `<image> <bin_path> <args>`.
///
/// The image itself carries no engine binary — `inputs.bin_path` is
/// bind-mounted read-only at its own host path and exec'd directly (#1653),
/// so the image only needs a compatible libc. The icebreaker-sealed
/// credential swap (#1651) happens in the caller, on `inputs.vars`, before
/// this is called.
///
/// The returned [`EnvFileGuard`] must outlive the spawned process — it
/// deletes the env file on drop, so dropping it before the container reads
/// its env would remove the file out from under `docker`/`podman`.
///
/// # Errors
/// See [`write_env_file`].
pub(crate) fn container_command(
    spec: &SandboxSpec,
    inputs: ContainerInputs<'_>,
) -> anyhow::Result<(std::process::Command, EnvFileGuard)> {
    // #1653/#1652 pre-pr-review P2: `--mount`'s value is a comma-delimited
    // `key=value` list with no escaping, and `spec.image` is appended as a
    // bare positional argument with no `--` separator — a path containing
    // `,`/`=`, or an image string starting with `-`, would silently splice
    // in an extra mount option or get parsed as a runtime flag instead of an
    // image name (potentially neutralizing the hardening flags below).
    // `spec.image` comes straight from `features.sandbox.image` in
    // `config.yaml`; the paths are host-derived but not implausible for a
    // local user to control (e.g. an oddly-named project directory).
    validate_mount_path(inputs.project_dir)?;
    if let Some(sock) = inputs.ssh_auth_sock {
        validate_mount_path(sock)?;
    }
    if let Some(dir) = inputs.config_dir {
        validate_mount_path(dir)?;
    }
    if let Some(patched) = inputs.patched_mcp_config {
        validate_mount_path(patched)?;
    }
    if let Some(cred) = inputs.credential_file {
        validate_mount_path(cred)?;
    }
    validate_mount_path(inputs.bin_path)?;
    anyhow::ensure!(
        !spec.image.starts_with('-'),
        "features.sandbox.image '{}' starts with '-', which {} would parse as a flag rather \
         than an image name",
        spec.image,
        spec.runtime.binary_name(),
    );

    let env_file = write_env_file(inputs.vars)?;

    let mut cmd = std::process::Command::new(spec.runtime.binary_name());
    cmd.arg("run").arg("--rm");
    cmd.arg("--cap-drop=ALL")
        .arg("--security-opt=no-new-privileges");
    let (uid, gid) = (rustix::process::getuid(), rustix::process::getgid());
    cmd.arg("--user")
        .arg(format!("{}:{}", uid.as_raw(), gid.as_raw()));
    match spec.runtime {
        ContainerRuntime::Podman => {
            // Maps the container's remapped uid back to the host's, so files
            // written under the bind-mounted workspace land owned by the
            // launching user rather than a subuid-mapped stranger. Docker has
            // no equivalent flag; `--user` alone is docker's whole story here.
            cmd.arg("--userns=keep-id");
        }
        ContainerRuntime::Docker => {
            // Required on Linux for `host.docker.internal` (the address
            // icebreaker's credential proxy is reached at — see
            // `icebreaker::gateway_host`) to resolve inside the container;
            // Docker Desktop already provides it without this flag, and a
            // redundant --add-host there is harmless.
            cmd.arg("--add-host=host.docker.internal:host-gateway");
        }
    }
    cmd.arg("--mount").arg(format!(
        "type=bind,source={},target={WORKSPACE_PATH}",
        inputs.project_dir.display()
    ));
    cmd.arg("-w").arg(WORKSPACE_PATH);
    if let Some(sock) = inputs.ssh_auth_sock {
        cmd.arg("--mount").arg(format!(
            "type=bind,source={},target={SSH_AUTH_SOCK_PATH},readonly",
            sock.display()
        ));
        cmd.arg("--env")
            .arg(format!("SSH_AUTH_SOCK={SSH_AUTH_SOCK_PATH}"));
    }
    if let Some(dir) = inputs.config_dir {
        cmd.arg("--mount").arg(format!(
            "type=bind,source={},target={},readonly",
            dir.display(),
            dir.display(),
        ));
        // #1652 pre-pr-review P0, generalized in #1698: the directory mount
        // above exposes any runtime-written credential file the adapter
        // keeps outside its own materialized config — Claude Code's
        // plaintext `.credentials.json` OAuth/API-key store on Linux (and on
        // macOS whenever the keychain backend is disabled), Codex's
        // `auth.json`. That file is the engine's own runtime secret, never
        // something llmenv wrote for the container to read — mask it with a
        // `/dev/null` overlay (an always-empty file at that path) so the
        // mounted directory carries the adapter's ordinary config without
        // also handing the container a live credential. `None` for an
        // adapter with no such file (Crush, opencode).
        if let Some(cred) = inputs.credential_file {
            cmd.arg("--mount").arg(format!(
                "type=bind,source=/dev/null,target={},readonly",
                cred.display(),
            ));
        }
        // Overlay-mounted AFTER the directory mount so it shadows the real
        // file at the same path inside the container — mount order matters
        // here, both engines apply mounts in the order given.
        if let (Some(patched), Some(target)) = (inputs.patched_mcp_config, inputs.mcp_config_path) {
            cmd.arg("--mount").arg(format!(
                "type=bind,source={},target={},readonly",
                patched.display(),
                target.display(),
            ));
        }
    }
    // #1653: the image doesn't carry the engine binary — bind-mount the
    // resolved host binary read-only at the identical in-container path and
    // exec that path directly, so any image with a compatible libc works
    // regardless of what's on its own `PATH`.
    cmd.arg("--mount").arg(format!(
        "type=bind,source={},target={},readonly",
        inputs.bin_path.display(),
        inputs.bin_path.display(),
    ));
    cmd.arg("--env-file").arg(&env_file.0);
    cmd.arg(&spec.image);
    cmd.arg(inputs.bin_path);
    cmd.args(inputs.args);
    Ok((cmd, env_file))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path_with(dir: &std::path::Path) -> std::ffi::OsString {
        std::env::join_paths([dir]).unwrap()
    }

    // manifest_inspect_target
    #[test]
    fn manifest_inspect_target_is_bare_for_docker() {
        assert_eq!(
            manifest_inspect_target(ContainerRuntime::Docker, "alpine:latest"),
            "alpine:latest"
        );
    }

    #[test]
    fn manifest_inspect_target_adds_docker_transport_for_podman() {
        assert_eq!(
            manifest_inspect_target(ContainerRuntime::Podman, "alpine:latest"),
            "docker://alpine:latest"
        );
    }

    fn make_executable(dir: &std::path::Path, name: &str) {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn auto_prefers_podman_over_docker_when_both_present() {
        let dir = tempfile::tempdir().unwrap();
        make_executable(dir.path(), "podman");
        make_executable(dir.path(), "docker");
        let path_var = path_with(dir.path());
        assert_eq!(
            resolve_runtime_in_path_list(&SandboxRuntime::Auto, &path_var),
            Some(ContainerRuntime::Podman)
        );
    }

    #[test]
    fn auto_falls_back_to_docker_when_podman_absent() {
        let dir = tempfile::tempdir().unwrap();
        make_executable(dir.path(), "docker");
        let path_var = path_with(dir.path());
        assert_eq!(
            resolve_runtime_in_path_list(&SandboxRuntime::Auto, &path_var),
            Some(ContainerRuntime::Docker)
        );
    }

    #[test]
    fn auto_returns_none_when_neither_present() {
        let dir = tempfile::tempdir().unwrap();
        let path_var = path_with(dir.path());
        assert_eq!(
            resolve_runtime_in_path_list(&SandboxRuntime::Auto, &path_var),
            None
        );
    }

    #[test]
    fn forced_docker_ignores_podman_even_when_present() {
        let dir = tempfile::tempdir().unwrap();
        make_executable(dir.path(), "podman");
        make_executable(dir.path(), "docker");
        let path_var = path_with(dir.path());
        assert_eq!(
            resolve_runtime_in_path_list(&SandboxRuntime::Docker, &path_var),
            Some(ContainerRuntime::Docker)
        );
    }

    #[test]
    fn forced_podman_returns_none_when_only_docker_present() {
        let dir = tempfile::tempdir().unwrap();
        make_executable(dir.path(), "docker");
        let path_var = path_with(dir.path());
        assert_eq!(
            resolve_runtime_in_path_list(&SandboxRuntime::Podman, &path_var),
            None
        );
    }

    #[test]
    fn binary_name_matches_the_probed_name() {
        assert_eq!(ContainerRuntime::Docker.binary_name(), "docker");
        assert_eq!(ContainerRuntime::Podman.binary_name(), "podman");
    }

    #[test]
    fn requested_binaries_names_both_for_auto() {
        assert_eq!(
            requested_binaries(&SandboxRuntime::Auto),
            "podman or docker"
        );
        assert_eq!(requested_binaries(&SandboxRuntime::Docker), "docker");
        assert_eq!(requested_binaries(&SandboxRuntime::Podman), "podman");
    }

    fn command_args(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn read_env_file(args: &[String]) -> String {
        let idx = args.iter().position(|a| a == "--env-file").unwrap();
        std::fs::read_to_string(&args[idx + 1]).unwrap()
    }

    #[test]
    fn container_command_builds_run_rm_workspace_mount_and_image_binary_args() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Podman,
            image: "registry.example.com/sandbox:latest".to_string(),
            forward_ssh_agent: true,
        };
        let args = vec!["--foo".to_string(), "bar".to_string()];
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/home/user/project");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        assert_eq!(cmd.get_program(), "podman");
        let cmd_args = command_args(&cmd);
        assert!(cmd_args.contains(&"--mount".to_string()));
        assert!(
            cmd_args.contains(&"type=bind,source=/home/user/project,target=/workspace".to_string())
        );
        assert!(cmd_args.contains(&"--userns=keep-id".to_string()));
        assert!(cmd_args.contains(&"--cap-drop=ALL".to_string()));
        assert!(cmd_args.contains(&"--security-opt=no-new-privileges".to_string()));
        // Tail: image, binary, args — unaffected by the mount/env-file rework.
        let tail = &cmd_args[cmd_args.len() - 4..];
        assert_eq!(
            tail,
            &[
                "registry.example.com/sandbox:latest",
                "/usr/local/bin/claude",
                "--foo",
                "bar",
            ]
        );
    }

    #[test]
    fn container_command_mounts_the_engine_binary_read_only_at_the_identical_path() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let bin_path = std::path::Path::new("/home/user/.local/bin/claude");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path,
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(cmd_args.contains(&format!(
            "type=bind,source={p},target={p},readonly",
            p = bin_path.display()
        )));
    }

    #[test]
    fn container_command_mounts_ssh_auth_sock_read_only_when_present() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let ssh_sock = std::path::Path::new("/tmp/ssh-XXXX/agent.1");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: Some(ssh_sock),
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        let args = command_args(&cmd);
        assert!(args.contains(
            &"type=bind,source=/tmp/ssh-XXXX/agent.1,target=/run/llmenv-ssh-agent.sock,readonly"
                .to_string()
        ));
        assert!(args.contains(&"SSH_AUTH_SOCK=/run/llmenv-ssh-agent.sock".to_string()));
        // Docker gets no --userns flag — that's podman-only.
        assert!(!args.contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn container_command_rejects_a_project_dir_containing_a_comma() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj,evil=target");
        let err = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot represent safely"));
    }

    #[test]
    fn container_command_rejects_an_image_starting_with_a_dash() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "--privileged".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let err = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("starts with '-'"));
    }

    #[test]
    fn container_command_writes_resolved_vars_to_the_env_file() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let mut vars = BTreeMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        let project_dir = std::path::Path::new("/proj");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(
            !cmd_args.iter().any(|a| a.contains("FOO=bar")),
            "the raw value must not appear in argv"
        );
        assert_eq!(read_env_file(&cmd_args), "FOO=bar\n");
    }

    #[test]
    fn write_env_file_writes_under_llmenv_state_dir_not_the_shared_os_temp_dir() {
        let mut vars = BTreeMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        let guard = write_env_file(&vars).unwrap();
        let state_dir = crate::paths::state_dir().unwrap();
        assert!(
            guard.0.starts_with(&state_dir),
            "env file {} must live under llmenv's own state dir {}",
            guard.0.display(),
            state_dir.display()
        );
        assert!(
            !guard.0.starts_with(std::env::temp_dir()),
            "env file {} must not live in the shared OS temp dir",
            guard.0.display()
        );
    }

    /// Spawn and immediately reap a child, returning a pid guaranteed dead at
    /// the moment this returns — used to test the sweep's liveness check
    /// without depending on a pid that just happens to be free.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn pid_from_sandbox_tmp_name_parses_the_env_file_shape() {
        assert_eq!(
            pid_from_sandbox_tmp_name("llmenv-sandbox-1234-5.env"),
            Some(1234)
        );
    }

    #[test]
    fn pid_from_sandbox_tmp_name_parses_the_patched_claude_json_shape() {
        assert_eq!(
            pid_from_sandbox_tmp_name("llmenv-sandbox-claude-json-1234-5"),
            Some(1234)
        );
    }

    #[test]
    fn pid_from_sandbox_tmp_name_returns_none_for_an_unparseable_shape() {
        assert_eq!(pid_from_sandbox_tmp_name("llmenv-sandbox-not-a-pid"), None);
    }

    #[test]
    fn process_is_alive_is_true_for_the_current_process() {
        assert!(process_is_alive(std::process::id()));
    }

    #[test]
    fn process_is_alive_is_false_for_a_reaped_child() {
        assert!(!process_is_alive(dead_pid()));
    }

    #[test]
    fn sweep_stale_removes_a_dead_pids_file_even_when_freshly_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("llmenv-sandbox-{}-0.env", dead_pid()));
        std::fs::write(&path, b"x").unwrap();

        sweep_stale(dir.path(), std::time::Duration::from_secs(3600));

        assert!(
            !path.exists(),
            "a dead pid's file must be swept regardless of age"
        );
    }

    #[test]
    fn sweep_stale_keeps_a_live_pids_file_even_when_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("llmenv-sandbox-{}-0.env", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();

        sweep_stale(dir.path(), std::time::Duration::from_secs(3600));

        assert!(
            path.exists(),
            "a live session's file must never be swept just for being old \
             (its patched claude.json can stay bind-mounted for the whole session)"
        );
    }

    #[test]
    fn sweep_stale_never_touches_an_entry_outside_its_own_naming_convention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-llmenvs-file.txt");
        std::fs::write(&path, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();

        sweep_stale(dir.path(), std::time::Duration::from_secs(3600));

        assert!(path.exists(), "an unrelated file must never be swept");
    }

    #[test]
    fn sweep_stale_falls_back_to_age_when_the_pid_segment_is_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("llmenv-sandbox-not-a-pid-shape");
        let fresh = dir.path().join("llmenv-sandbox-also-not-a-pid-shape");
        std::fs::write(&stale, b"x").unwrap();
        std::fs::write(&fresh, b"y").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old)).unwrap();

        sweep_stale(dir.path(), std::time::Duration::from_secs(3600));

        assert!(
            !stale.exists(),
            "an unparseable-shape file past the age threshold is swept"
        );
        assert!(
            fresh.exists(),
            "an unparseable-shape file within the age threshold is kept"
        );
    }

    #[test]
    fn sandbox_tmp_dir_refuses_a_symlinked_target() {
        let base = tempfile::tempdir().unwrap();
        let real_target = base.path().join("real");
        std::fs::create_dir(&real_target).unwrap();
        let link = base.path().join("sandbox-tmp");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let err = prepare_sandbox_tmp_dir(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[test]
    fn container_command_mounts_config_dir_read_only_at_the_identical_path() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let config_dir = std::path::Path::new("/home/user/.cache/llmenv/claude-code/abc123");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: Some(config_dir),
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(cmd_args.contains(&format!(
            "type=bind,source={p},target={p},readonly",
            p = config_dir.display()
        )));
    }

    #[test]
    fn container_command_masks_credential_file_with_dev_null_when_present() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let config_dir = std::path::Path::new("/home/user/.cache/llmenv/claude-code/abc123");
        let credential_file = config_dir.join(".credentials.json");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: Some(config_dir),
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: Some(&credential_file),
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(cmd_args.contains(&format!(
            "type=bind,source=/dev/null,target={}/.credentials.json,readonly",
            config_dir.display()
        )));
    }

    #[test]
    fn container_command_mounts_no_credential_mask_when_adapter_has_none() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let config_dir = std::path::Path::new("/home/user/.cache/llmenv/crush/abc123");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/crush"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: Some(config_dir),
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(!cmd_args.iter().any(|a| a.contains("/dev/null")));
    }

    #[test]
    fn container_command_overlays_patched_mcp_config_over_the_config_dir_mount() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let config_dir = std::path::Path::new("/home/user/.cache/llmenv/claude-code/abc123");
        let patched = std::path::Path::new("/tmp/llmenv-sandbox-mcp-config-42-0");
        let mcp_config_path = config_dir.join(".claude.json");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: Some(config_dir),
                mcp_config_path: Some(&mcp_config_path),
                patched_mcp_config: Some(patched),
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(cmd_args.contains(&format!(
            "type=bind,source={},target={}/.claude.json,readonly",
            patched.display(),
            config_dir.display()
        )));
    }

    #[test]
    fn container_command_overlays_patched_mcp_config_for_a_non_claude_code_file_name() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let config_dir = std::path::Path::new("/home/user/.cache/llmenv/codex/abc123");
        let patched = std::path::Path::new("/tmp/llmenv-sandbox-mcp-config-42-0");
        let mcp_config_path = config_dir.join("config.toml");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/codex"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: Some(config_dir),
                mcp_config_path: Some(&mcp_config_path),
                patched_mcp_config: Some(patched),
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(cmd_args.contains(&format!(
            "type=bind,source={},target={}/config.toml,readonly",
            patched.display(),
            config_dir.display()
        )));
    }

    #[test]
    fn container_command_mounts_nothing_for_config_dir_when_absent() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: None,
                mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(!cmd_args.iter().any(|a| a.contains(".claude.json")));
        assert!(!cmd_args.iter().any(|a| a.contains(".credentials.json")));
    }

    proptest! {
        #[test]
        fn prop_resolve_runtime_matches_decision_table(
            podman_present in proptest::bool::ANY,
            docker_present in proptest::bool::ANY,
            configured in prop_oneof![
                Just(SandboxRuntime::Auto),
                Just(SandboxRuntime::Docker),
                Just(SandboxRuntime::Podman),
            ],
        ) {
            let dir = tempfile::tempdir().unwrap();
            if podman_present {
                make_executable(dir.path(), "podman");
            }
            if docker_present {
                make_executable(dir.path(), "docker");
            }
            let path_var = path_with(dir.path());
            let result = resolve_runtime_in_path_list(&configured, &path_var);
            let expected = match configured {
                SandboxRuntime::Docker => docker_present.then_some(ContainerRuntime::Docker),
                SandboxRuntime::Podman => podman_present.then_some(ContainerRuntime::Podman),
                SandboxRuntime::Auto => {
                    if podman_present {
                        Some(ContainerRuntime::Podman)
                    } else if docker_present {
                        Some(ContainerRuntime::Docker)
                    } else {
                        None
                    }
                }
            };
            prop_assert_eq!(result, expected);
        }

        #[test]
        fn prop_container_command_forwards_all_vars_and_preserves_tail_order(
            vars in proptest::collection::btree_map("[A-Z_]{1,10}", "[a-zA-Z0-9]{0,15}", 0..5),
            extra_args in proptest::collection::vec("[a-zA-Z0-9-]{1,10}", 0..4),
            binary_name in "[a-z]{1,10}",
            // First char excludes '-' — container_command now rejects an
            // image starting with '-' (pre-pr-review P2, #1653), covered by
            // its own dedicated test rather than this general property.
            image in "[a-z0-9./:][a-z0-9./:-]{0,29}",
        ) {
            let spec = SandboxSpec {
                runtime: ContainerRuntime::Docker,
                image: image.clone(),
                forward_ssh_agent: true,
            };
            let project_dir = std::path::Path::new("/proj");
            let (cmd, _guard) = container_command(
                &spec,
                ContainerInputs {
                    bin_path: std::path::Path::new(&binary_name),
                    args: &extra_args,
                    vars: &vars,
                    project_dir,
                    ssh_auth_sock: None,
                    config_dir: None,
                    mcp_config_path: None,
                patched_mcp_config: None,
                credential_file: None,
                },
            )
            .unwrap();
            let cmd_args = command_args(&cmd);
            prop_assert!(cmd_args.contains(&"--mount".to_string()));
            let env_file_content = read_env_file(&cmd_args);
            for (k, v) in &vars {
                let line = format!("{k}={v}\n");
                prop_assert!(env_file_content.contains(&line));
            }
            let tail_len = 2 + extra_args.len();
            let tail = &cmd_args[cmd_args.len() - tail_len..];
            prop_assert_eq!(&tail[0], &image);
            prop_assert_eq!(&tail[1], &binary_name);
            prop_assert_eq!(&tail[2..], extra_args.as_slice());
        }
    }
}
