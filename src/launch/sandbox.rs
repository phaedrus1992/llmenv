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
    /// read-only at the identical in-container path (#1652), so
    /// `mcpServers`/skills/plugins/settings are visible to the containerized
    /// engine. `None` for a non-Claude-Code adapter or when no config
    /// directory was resolved.
    pub(crate) config_dir: Option<&'a std::path::Path>,
    /// Host path to a patched copy of `.claude.json` (ICM's mcpServers URL
    /// rewritten from loopback to the container gateway host, #1652),
    /// overlay-mounted read-only over the real file inside `config_dir` so
    /// the rewritten URL wins without touching the host's own file. `None`
    /// when no ICM entry needed rewriting, or `config_dir` is `None`.
    pub(crate) patched_claude_json: Option<&'a std::path::Path>,
}

/// An env file written for one `docker`/`podman` invocation, deleted on drop
/// once that invocation (and the container it started) has exited — mirrors
/// `SocketCleanup`'s pattern for the launch notice socket.
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
    let path = std::env::temp_dir().join(format!("llmenv-sandbox-{}-{n}.env", std::process::id()));
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
        // Overlay-mounted AFTER the directory mount so it shadows the real
        // file at the same path inside the container — mount order matters
        // here, both engines apply mounts in the order given.
        if let Some(patched) = inputs.patched_claude_json {
            cmd.arg("--mount").arg(format!(
                "type=bind,source={},target={},readonly",
                patched.display(),
                dir.join(".claude.json").display(),
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
                patched_claude_json: None,
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
                patched_claude_json: None,
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
                patched_claude_json: None,
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
                patched_claude_json: None,
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
                patched_claude_json: None,
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
    fn container_command_overlays_patched_claude_json_over_the_config_dir_mount() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
            forward_ssh_agent: true,
        };
        let args = Vec::new();
        let vars = BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let config_dir = std::path::Path::new("/home/user/.cache/llmenv/claude-code/abc123");
        let patched = std::path::Path::new("/tmp/llmenv-sandbox-claude-json-42-0");
        let (cmd, _guard) = container_command(
            &spec,
            ContainerInputs {
                bin_path: std::path::Path::new("/usr/local/bin/claude"),
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
                config_dir: Some(config_dir),
                patched_claude_json: Some(patched),
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
                patched_claude_json: None,
            },
        )
        .unwrap();
        let cmd_args = command_args(&cmd);
        assert!(!cmd_args.iter().any(|a| a.contains(".claude.json")));
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
            image in "[a-z0-9./:-]{1,30}",
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
                    patched_claude_json: None,
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
