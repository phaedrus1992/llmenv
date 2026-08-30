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
//! GitHub, still launches — only a `gh` that *reached* a definitive answer
//! (a matching attestation, or a confirmed absence of one) and reported
//! failure blocks the launch.
//!
//! Only ever runs against llmenv's own published sandbox image
//! (`ghcr.io/phaedrus1992/llmenv-sandbox`) — `gh attestation verify` checks
//! whether an image's attestation was signed by a specific repository, so
//! running it against a `features.sandbox.image` override pointing at some
//! other image would always fail (that image was never attested by this
//! repo) and block every launch for no security benefit.
//!
//! Residual limitation: `gh attestation verify` itself depends on GitHub's
//! API and Sigstore's transparency log (Rekor) in addition to the image
//! registry. An attacker in a position to selectively block just those
//! endpoints — while leaving the registry pull path the container runtime
//! uses fully reachable — could still cause verification to skip while a
//! substituted image gets pulled and run. `--bundle-from-oci` below narrows
//! this by sourcing the attestation bundle from the same registry as the
//! image itself rather than GitHub's API, but does not remove the
//! Sigstore/Rekor dependency; closing that gap fully would need offline
//! verification against a locally pinned trust root, tracked separately.

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// GitHub repository the sandbox image's attestation must be signed by.
const SANDBOX_IMAGE_REPO: &str = "phaedrus1992/llmenv";

/// Workflow path `gh attestation verify --signer-workflow` pins the check
/// to, so an attestation from some other workflow in the same repo (one
/// with `attestations: write` for an unrelated reason) doesn't satisfy it —
/// `gh --help` calls this out explicitly as the identity check `--repo`
/// alone doesn't provide.
const SANDBOX_IMAGE_SIGNER_WORKFLOW: &str =
    "phaedrus1992/llmenv/.github/workflows/sandbox-image.yml";

/// Registry + repository prefix of llmenv's own published sandbox image.
/// Verification only runs when `spec.image` starts with this — a
/// `features.sandbox.image` override pointing at any other image was never
/// attested by [`SANDBOX_IMAGE_REPO`], so checking it would always fail
/// closed for a reason that has nothing to do with the image's legitimacy.
const SANDBOX_IMAGE_PREFIX: &str = "ghcr.io/phaedrus1992/llmenv-sandbox@";

/// How long to wait for `gh attestation verify` before treating it as hung
/// and skipping verification. Matches `icebreaker.rs`'s `SUBPROCESS_TIMEOUT`
/// — this runs before `launch`'s tokio runtime exists (`resolve_sandbox_spec`
/// is called from sync code ahead of `mod.rs`'s `Builder::new_current_thread`),
/// so it can't reuse `tokio::time::timeout` and instead drains the child's
/// stdout/stderr on reader threads while polling `try_wait` on the caller's
/// thread, to keep an unread pipe from deadlocking the child (`gh`'s success
/// output — the full attestation bundle — is well over a typical pipe
/// buffer's size).
const GH_TIMEOUT: Duration = Duration::from_secs(10);

/// stderr substrings meaning `gh` never reached GitHub's API, the image
/// registry, or Sigstore's transparency log at all (DNS/connect/TLS-level
/// failure) — as opposed to reaching one of them and getting an answer.
/// Matched case-insensitively. This list, and [`INCONCLUSIVE_RESPONSE_MARKERS`]
/// below, are both best-effort: `gh` has no dedicated "offline" exit code, so
/// a failure matching neither is treated as a genuine, definitive
/// verification failure (fail closed) rather than silently skipped.
const NETWORK_UNREACHABLE_MARKERS: &[&str] = &[
    "could not resolve host",
    "no such host",
    "connection refused",
    "network is unreachable",
    "i/o timeout",
    "dial tcp",
    "tls handshake",
];

/// stderr substrings meaning `gh` reached a server but got an inconclusive
/// answer unrelated to whether the image itself is legitimate — rate
/// limiting or a registry/API-side outage. An unauthenticated `gh` (no
/// `gh auth login` on this machine) is especially likely to hit GitHub's
/// unauthenticated API rate limit here; treating that the same as a
/// confirmed "no attestation" result would block every launch on a
/// gh-installed-but-not-logged-in machine whenever the limit is already
/// exhausted, for a reason that says nothing about the image.
const INCONCLUSIVE_RESPONSE_MARKERS: &[&str] = &[
    "rate limit",
    "http 403",
    "http 500",
    "http 502",
    "http 503",
    "http 504",
];

/// Verify `image`'s build-provenance attestation was signed by
/// [`SANDBOX_IMAGE_SIGNER_WORKFLOW`] before it runs. Only runs at all when
/// `image` is llmenv's own published sandbox image — see the module doc.
///
/// # Errors
/// Only when `gh` ran, reached a definitive answer, and reported the image
/// failed verification. A missing `gh` binary, a hung `gh`, an unreachable
/// network, or an inconclusive response (rate limit, registry/API outage)
/// are all logged and treated as "skip, allow the launch" — see the module
/// doc.
pub(crate) fn verify_before_run(image: &str) -> anyhow::Result<()> {
    if !image.starts_with(SANDBOX_IMAGE_PREFIX) {
        return Ok(());
    }
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

/// Runs the real `gh attestation verify` invocation, bounded by
/// [`GH_TIMEOUT`].
fn run_gh_attestation_verify(image: &str, repo: &str) -> std::io::Result<Output> {
    let mut cmd = Command::new("gh");
    cmd.args(["attestation", "verify"])
        .arg(format!("oci://{image}"))
        .args(["--repo", repo])
        .args(["--signer-workflow", SANDBOX_IMAGE_SIGNER_WORKFLOW])
        .arg("--bundle-from-oci")
        .args(["--format", "json"]);
    run_with_timeout(cmd, GH_TIMEOUT)
}

/// Runs `cmd` to completion, killing it and returning
/// [`std::io::ErrorKind::TimedOut`] if it doesn't finish within `timeout`.
///
/// Drains stdout/stderr on separate reader threads rather than reading them
/// after the child exits — `Command::output()`'s own approach — because a
/// child whose output exceeds the OS pipe buffer (an attestation bundle
/// well over 64KB is common here) blocks on write until something reads,
/// which a bare `try_wait` poll loop never does.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (Some(mut stdout_pipe), Some(mut stderr_pipe)) = (child.stdout.take(), child.stderr.take())
    else {
        return Err(std::io::Error::other(
            "gh: piped stdout/stderr unexpectedly absent after spawn",
        ));
    };
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("gh attestation verify did not finish within {timeout:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Output {
        status,
        stdout: stdout_thread.join().unwrap_or_default(),
        stderr: stderr_thread.join().unwrap_or_default(),
    })
}

/// Classification logic shared by [`verify_before_run_in`]'s "gh is on
/// PATH" branch: run `gh attestation verify`, and decide whether a failure
/// means "block the launch" or "gh didn't reach a definitive answer, skip".
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
    let inconclusive = NETWORK_UNREACHABLE_MARKERS
        .iter()
        .chain(INCONCLUSIVE_RESPONSE_MARKERS)
        .any(|m| lower.contains(m));
    if inconclusive {
        eprintln!(
            "llmenv: gh attestation verify got no definitive answer — skipping sandbox image \
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
    fn skips_when_gh_reports_rate_limiting() {
        let result = verify_with_gh("img", "owner/repo", |_, _| {
            output(false, "HTTP 403: API rate limit exceeded for 203.0.113.5.")
        });
        assert!(result.is_ok());
    }

    #[test]
    fn skips_when_the_registry_returns_a_server_error() {
        let result = verify_with_gh("img", "owner/repo", |_, _| {
            output(
                false,
                "failed to fetch remote image: HTTP 503: Service Unavailable",
            )
        });
        assert!(result.is_ok());
    }

    #[test]
    fn fails_when_gh_reports_a_genuine_verification_failure() {
        let result = verify_with_gh("img", "owner/repo", |_, _| {
            output(
                false,
                "Error: HTTP 404: Not Found (.../attestations/sha256:...)",
            )
        });
        let err = result.expect_err("a real verification failure must block the launch");
        assert!(
            err.to_string()
                .contains("failed build-provenance attestation verification"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn skips_verification_for_a_non_default_image() {
        let result = super::verify_before_run("registry.example.com/some/other-image:latest");
        assert!(result.is_ok());
    }
}
