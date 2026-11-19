//! The `gh pr view` call of the `pr` widget and the parse of its answer.

use serde::Deserialize;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::gh_cmd::GhPrCmd;
use super::gh_lookup::{GhFailure, classify_gh_exit};
use super::{PrInfo, PrLookup};

/// Shell out to `gh pr view --json number,url,reviewDecision -- <branch>` in
/// `repo_dir` and parse the result. `Ok(None)` for no open PR, no remote, or
/// unparseable output. A missing binary, a failed login, a timeout
/// (`gh.timeout`), and a failed read or wait are failures, so the widget can name
/// them instead of showing "no PR" (#2821). This never prints or panics.
pub(super) fn gh_pr_view(gh: GhPrCmd<'_>, repo_dir: &Path, branch: &str) -> PrLookup {
    // The repository is the working folder, so a bare `gh` could run a copy it holds (#2948).
    let Ok(program) = crate::path_lookup::resolve_program(gh.bin) else {
        return Err(GhFailure::Missing);
    };
    // `--` terminates option parsing so `branch` (derived from git state) is
    // always read as the positional argument, never as a `gh` flag. The flags
    // stay before `--`; only the positional goes after it.
    let build = || {
        let mut cmd = Command::new(&program);
        cmd.args([
            "pr",
            "view",
            "--json",
            "number,url,reviewDecision",
            "--",
            branch,
        ])
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        cmd
    };
    let run = || crate::process::run_traced("statusline:gh-pr-view", build(), gh.timeout);
    // One retry after a brief backoff (#1062): a failure to start under heavy
    // concurrent process load is transient, not "gh isn't installed".
    // A timeout is final, and a missing binary fails the retry too.
    let out = match run() {
        Err(e) if !matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::NotFound) => {
            // The first failure is logged here, so a retry that works still leaves a trace (#3036).
            tracing::warn!("gh pr view failed, so it is retried once: {e}");
            std::thread::sleep(Duration::from_millis(20));
            run().inspect_err(|second| {
                tracing::error!(
                    "gh pr view failed twice: {e}, then {second}. Fix: run `gh pr view` by hand \
                     in this folder to see the cause."
                );
            })
        }
        other => other,
    };
    match out {
        Ok(out) if out.status.success() => Ok(parse_gh_pr_view(&out.stdout)),
        Ok(out) => classify_gh_exit(out.status, &String::from_utf8_lossy(&out.stderr)),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            tracing::debug!("gh pr view: gh is not installed: {e}");
            Err(GhFailure::Missing)
        }
        Err(e) if e.kind() == ErrorKind::TimedOut => {
            let secs = gh.timeout.as_secs_f32();
            tracing::error!(
                "gh pr view timed out after {secs}s. Fix: run `gh pr view` by hand in this \
                 folder, and check that GitHub is reachable."
            );
            Err(GhFailure::TimedOut)
        }
        Err(e) => {
            tracing::error!("gh pr view: {e}");
            Err(GhFailure::Failed)
        }
    }
}

/// Parse `gh pr view --json number,url,reviewDecision`'s stdout into a
/// [`PrInfo`], mapping `gh`'s `reviewDecision` (`"APPROVED"` /
/// `"CHANGES_REQUESTED"` / `"REVIEW_REQUIRED"` / absent) onto the same
/// lowercase `review_state` values an engine-supplied PR uses.
pub(super) fn parse_gh_pr_view(stdout: &[u8]) -> Option<PrInfo> {
    #[derive(Deserialize)]
    struct GhPrView {
        number: Option<u64>,
        url: Option<String>,
        #[serde(rename = "reviewDecision")]
        review_decision: Option<String>,
    }
    let parsed: GhPrView = match serde_json::from_slice(stdout) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                "gh pr view printed JSON that does not parse, so the PR is unknown: {e}. Fix: run `gh pr view` by hand in this folder."
            );
            return None;
        }
    };
    parsed.number?;
    Some(PrInfo {
        number: parsed.number,
        url: parsed.url,
        review_state: parsed
            .review_decision
            .as_deref()
            .and_then(map_review_decision)
            .map(str::to_string),
    })
}

/// Map `gh`'s `reviewDecision` enum values to the lowercase snake_case
/// `review_state` strings [`pr_style_for`] matches on. `gh` never emits a
/// `"pending"` decision (that value exists only for a possible future
/// engine-supplied `review_state`) — an absent/unrecognized decision maps to
/// `None`, rendering with the widget's default color.
pub(super) fn map_review_decision(decision: &str) -> Option<&'static str> {
    match decision {
        "APPROVED" => Some("approved"),
        "CHANGES_REQUESTED" => Some("changes_requested"),
        "REVIEW_REQUIRED" => Some("review_required"),
        _ => None,
    }
}
