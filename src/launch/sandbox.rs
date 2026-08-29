//! Sandboxed `llmenv launch` (#1080): resolve which container engine to use,
//! and (later issues in this design) build and run the container itself. See
//! `docs/design/issue-1080-sidecar-container.md`.

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

/// Resolved container settings for a sandboxed launch — the concrete engine
/// and image to run, after `Auto`/override resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxSpec {
    pub(crate) runtime: ContainerRuntime,
    pub(crate) image: String,
}

/// In-container path the project tree is mounted at (#1650) — the
/// devcontainer convention the design doc calls out.
pub(crate) const WORKSPACE_PATH: &str = "/workspace";

/// In-container path the host's `SSH_AUTH_SOCK` is mounted at (#1650), when
/// present. Namespaced under `llmenv-` so it can't collide with a path the
/// image itself already uses.
pub(crate) const SSH_AUTH_SOCK_PATH: &str = "/run/llmenv-ssh-agent.sock";

/// Everything [`container_command`] needs besides the resolved
/// [`SandboxSpec`] — bundled so the function stays inside the
/// 5-positional-param limit.
pub(crate) struct ContainerInputs<'a> {
    pub(crate) binary_name: &'a str,
    pub(crate) args: &'a [String],
    /// The resolved/materialized env (#1650) — passed via repeated `-e
    /// KEY=VALUE` flags. `docker run`'s child is a fresh container
    /// namespace, so nothing set on the `docker`/`podman` CLI process's own
    /// environment reaches it automatically; this is deliberately not a live
    /// mount of `~/.config/llmenv` — the container gets the already-resolved
    /// result, not the source config tree.
    pub(crate) vars: &'a std::collections::BTreeMap<String, String>,
    /// Host directory bind-mounted read-write at [`WORKSPACE_PATH`].
    pub(crate) project_dir: &'a std::path::Path,
    /// Host `SSH_AUTH_SOCK`, if the launching shell has one running. Mounted
    /// read-only at [`SSH_AUTH_SOCK_PATH`] so the container can push over
    /// git without ever holding a private key.
    pub(crate) ssh_auth_sock: Option<&'a std::path::Path>,
}

/// Build the container invocation for a sandboxed launch (#1080):
/// `<runtime> run --rm` plus the project/SSH mounts and resolved env
/// (#1650), then `<image> <binary_name> <args>`.
///
/// The image is trusted to already carry `binary_name` on its own `PATH`;
/// llmenv does not yet copy the host's resolved engine binary into the
/// container (that's #1653's job). The icebreaker-sealed credential swap
/// (#1651) happens in the caller, on `inputs.vars`, before this is called.
pub(crate) fn container_command(
    spec: &SandboxSpec,
    inputs: ContainerInputs<'_>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(spec.runtime.binary_name());
    cmd.arg("run").arg("--rm");
    cmd.arg("-v").arg(format!(
        "{}:{WORKSPACE_PATH}:rw",
        inputs.project_dir.display()
    ));
    cmd.arg("-w").arg(WORKSPACE_PATH);
    if let Some(sock) = inputs.ssh_auth_sock {
        cmd.arg("-v")
            .arg(format!("{}:{SSH_AUTH_SOCK_PATH}:ro", sock.display()));
        cmd.arg("-e")
            .arg(format!("SSH_AUTH_SOCK={SSH_AUTH_SOCK_PATH}"));
    }
    for (key, value) in inputs.vars {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg(&spec.image);
    cmd.arg(inputs.binary_name);
    cmd.args(inputs.args);
    cmd
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

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

    #[test]
    fn container_command_builds_run_rm_workspace_mount_and_image_binary_args() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Podman,
            image: "registry.example.com/sandbox:latest".to_string(),
        };
        let args = vec!["--foo".to_string(), "bar".to_string()];
        let vars = std::collections::BTreeMap::new();
        let project_dir = std::path::Path::new("/home/user/project");
        let cmd = container_command(
            &spec,
            ContainerInputs {
                binary_name: "claude",
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
            },
        );
        assert_eq!(cmd.get_program(), "podman");
        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "--rm",
                "-v",
                "/home/user/project:/workspace:rw",
                "-w",
                "/workspace",
                "registry.example.com/sandbox:latest",
                "claude",
                "--foo",
                "bar",
            ]
        );
    }

    #[test]
    fn container_command_mounts_ssh_auth_sock_read_only_when_present() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
        };
        let args = Vec::new();
        let vars = std::collections::BTreeMap::new();
        let project_dir = std::path::Path::new("/proj");
        let ssh_sock = std::path::Path::new("/tmp/ssh-XXXX/agent.1");
        let cmd = container_command(
            &spec,
            ContainerInputs {
                binary_name: "claude",
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: Some(ssh_sock),
            },
        );
        let args = command_args(&cmd);
        assert!(args.contains(&"/tmp/ssh-XXXX/agent.1:/run/llmenv-ssh-agent.sock:ro".to_string()));
        assert!(args.contains(&"SSH_AUTH_SOCK=/run/llmenv-ssh-agent.sock".to_string()));
    }

    #[test]
    fn container_command_forwards_resolved_vars_as_e_flags() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Docker,
            image: "img".to_string(),
        };
        let args = Vec::new();
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        let project_dir = std::path::Path::new("/proj");
        let cmd = container_command(
            &spec,
            ContainerInputs {
                binary_name: "claude",
                args: &args,
                vars: &vars,
                project_dir,
                ssh_auth_sock: None,
            },
        );
        assert!(command_args(&cmd).contains(&"FOO=bar".to_string()));
    }
}
