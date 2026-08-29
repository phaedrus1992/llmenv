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

/// Build the container invocation for a sandboxed launch (#1080):
/// `<runtime> run --rm <image> <binary_name> <args>`.
///
/// Mounts and env forwarding are later issues (#1650, #1651) — this covers
/// the exec path only (#1649). The image is trusted to already carry
/// `binary_name` on its own `PATH`; llmenv does not yet copy the host's
/// resolved engine binary into the container (that's #1653's job).
pub(crate) fn container_command(
    spec: &SandboxSpec,
    binary_name: &str,
    args: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(spec.runtime.binary_name());
    cmd.arg("run").arg("--rm").arg(&spec.image).arg(binary_name);
    cmd.args(args);
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
    fn container_command_builds_run_rm_image_binary_args() {
        let spec = SandboxSpec {
            runtime: ContainerRuntime::Podman,
            image: "registry.example.com/sandbox:latest".to_string(),
        };
        let cmd = container_command(&spec, "claude", &["--foo".to_string(), "bar".to_string()]);
        assert_eq!(cmd.get_program(), "podman");
        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "--rm",
                "registry.example.com/sandbox:latest",
                "claude",
                "--foo",
                "bar",
            ]
        );
    }
}
