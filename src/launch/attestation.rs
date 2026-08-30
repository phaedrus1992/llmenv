//! Verifies a sandbox image's GitHub Actions build-provenance attestation
//! before `llmenv launch --container` runs it (#1719).
//! `.github/workflows/sandbox-image.yml` already produces one via
//! `actions/attest-build-provenance`; this confirms the pulled image is the
//! one that workflow actually built, not a substitute a compromised
//! registry account served under the same reference.
//!
//! Needs `gh` (the GitHub CLI) on `PATH` — chosen over adding a second
//! Sigstore client alongside cosign's own CI-side signing (#1723), since
//! `gh attestation verify` already ships this exact check with no extra
//! binary beyond a tool most contributors already have. Verification is
//! best-effort: a machine with no `gh` installed, or no network path to
//! GitHub, still launches — only a `gh` that *reached* GitHub and reported
//! a failed verification blocks the launch.

use std::ffi::OsStr;
use std::process::{Command, Output};

/// GitHub repository the sandbox image's attestation must be signed by.
const SANDBOX_IMAGE_REPO: &str = "phaedrus1992/llmenv";

/// Substrings in `gh`'s stderr that indicate it could not reach GitHub or
/// the image registry at all, as opposed to reaching them and finding the
/// image's attestation missing or invalid. Matched case-insensitively.
/// `gh` has no dedicated "offline" exit code, so this list is best-effort —
/// a failure that matches none of these is treated as a genuine
/// verification failure (fail closed) rather than silently skipped.
const NETWORK_UNREACHABLE_MARKERS: &[&str] = &[
    "could not resolve host",
    "no such host",
    "connection refused",
    "network is unreachable",
    "i/o timeout",
    "dial tcp",
];

/// Verify `image`'s build-provenance attestation was signed by
/// [`SANDBOX_IMAGE_REPO`]'s Actions workflow before it runs.
///
/// # Errors
/// Only when `gh` ran, reached GitHub, and reported the image failed
/// verification. A missing `gh` binary or an unreachable network are both
/// logged and treated as "skip, allow the launch" — see the module doc.
pub(crate) fn verify_before_run(image: &str) -> anyhow::Result<()> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    verify_before_run_in(
        image,
        SANDBOX_IMAGE_REPO,
        &path_var,
        run_gh_attestation_verify,
    )
}

/// [`verify_before_run`] against an injected `PATH` and `gh` invocation —
/// split out so both the "gh missing" and "gh ran" branches are testable
/// without spawning a real process or touching the network, mirroring
/// `sandbox::resolve_runtime_in_path_list`'s rationale for the same pattern.
fn verify_before_run_in(
    image: &str,
    repo: &str,
    path_var: &OsStr,
    run: impl Fn(&str, &str) -> std::io::Result<Output>,
) -> anyhow::Result<()> {
    if llmenv_paths::resolve_in_path_list("gh", path_var).is_none() {
        eprintln!(
            "llmenv: gh CLI not found on PATH — skipping sandbox image attestation \
             verification for {image}"
        );
        return Ok(());
    }
    verify_with_gh(image, repo, run)
}

/// Runs the real `gh attestation verify` invocation.
fn run_gh_attestation_verify(image: &str, repo: &str) -> std::io::Result<Output> {
    Command::new("gh")
        .args(["attestation", "verify"])
        .arg(format!("oci://{image}"))
        .args(["--repo", repo])
        .args(["--format", "json"])
        .output()
}

/// Classification logic shared by [`verify_before_run_in`]'s "gh is on
/// PATH" branch: run `gh attestation verify`, and decide whether a failure
/// means "block the launch" or "gh couldn't reach GitHub, skip".
fn verify_with_gh(
    image: &str,
    repo: &str,
    run: impl Fn(&str, &str) -> std::io::Result<Output>,
) -> anyhow::Result<()> {
    let output = match run(image, repo) {
        Ok(output) => output,
        Err(e) => {
            eprintln!(
                "llmenv: could not run gh attestation verify ({e}) — skipping sandbox image \
                 attestation verification for {image}"
            );
            return Ok(());
        }
    };
    if output.status.success() {
        tracing::debug!(image, "sandbox image attestation verified");
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_lowercase();
    if NETWORK_UNREACHABLE_MARKERS
        .iter()
        .any(|m| lower.contains(m))
    {
        eprintln!(
            "llmenv: gh attestation verify could not reach GitHub — skipping sandbox image \
             attestation verification for {image}: {}",
            stderr.trim()
        );
        return Ok(());
    }
    anyhow::bail!(
        "sandbox image {image} failed build-provenance attestation verification against \
         {repo}: {}",
        stderr.trim()
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::{verify_before_run_in, verify_with_gh};

    fn output(success: bool, stderr: &str) -> std::io::Result<std::process::Output> {
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn skips_when_gh_not_on_path() {
        let result = verify_before_run_in("img", "owner/repo", std::ffi::OsStr::new(""), |_, _| {
            panic!("run must not be called when gh is missing from PATH")
        });
        assert!(result.is_ok());
    }

    #[test]
    fn passes_when_gh_reports_success() {
        let result = verify_with_gh("img", "owner/repo", |_, _| output(true, ""));
        assert!(result.is_ok());
    }

    #[test]
    fn skips_when_gh_cannot_be_spawned() {
        let result = verify_with_gh("img", "owner/repo", |_, _| {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert!(result.is_ok());
    }

    #[test]
    fn skips_when_gh_reports_a_network_failure() {
        let result = verify_with_gh("img", "owner/repo", |_, _| {
            output(false, "dial tcp: lookup api.github.com: no such host")
        });
        assert!(result.is_ok());
    }

    #[test]
    fn fails_when_gh_reports_a_genuine_verification_failure() {
        let result = verify_with_gh("img", "owner/repo", |_, _| {
            output(
                false,
                "no attestations found matching the provided predicate",
            )
        });
        let err = result.expect_err("a real verification failure must block the launch");
        assert!(
            err.to_string()
                .contains("failed build-provenance attestation verification"),
            "unexpected error message: {err}"
        );
    }
}
