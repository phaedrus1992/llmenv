//! Tests of the `gh pr view` lookup and its exit classification, split out of `widget.rs` (#3036).

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use super::gh_lookup::{GH_LOGIN_SIGNS, GH_NO_PR_SIGNS};
use super::*;
use std::time::Duration;

#[test]
fn parse_gh_pr_view_none_for_missing_number_or_malformed_json() {
    assert_eq!(parse_gh_pr_view(br#"{"url":"https://x"}"#), None);
    assert_eq!(parse_gh_pr_view(b"not json"), None);
}

#[cfg(unix)]
pub(super) fn write_fake_gh(dir: &Path, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("gh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake-`gh` invocation with a deliberately generous timeout (#1073).
///
/// These tests assert what the derivation chain *does* with a subprocess's
/// output — not how quickly the OS schedules that subprocess. Under
/// `DEFAULT_GH_PR_TIMEOUT` a loaded machine can fail to run the `/bin/sh`
/// fake to completion inside the production budget, turning every such
/// test into a load-sensitive coin flip. The timeout's own behavior is
/// covered separately by
/// `gh_pr_view_fails_when_gh_outlives_its_timeout`.
#[cfg(unix)]
pub(super) fn fake_gh(bin: &str) -> GhPrCmd<'_> {
    GhPrCmd {
        bin,
        timeout: Duration::from_secs(120),
    }
}

#[cfg(unix)]
#[test]
fn gh_pr_view_derived_path_renders_expected_widget() {
    // Full derived-PR chain: a fake `gh` stands in for the real
    // subprocess, its JSON is parsed into a PrInfo, and that PrInfo
    // renders exactly like an engine-supplied one would.
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho '{\"number\":834,\"url\":\"https://github.com/o/r/pull/834\",\"reviewDecision\":\"CHANGES_REQUESTED\"}'\n",
    );
    let pr = gh_pr_view(fake_gh(&gh.to_string_lossy()), repo_dir.path(), "feat/x")
        .unwrap()
        .unwrap();
    assert_eq!(pr.number, Some(834));
    assert_eq!(render_pr_info(&pr, None, false), "#834");
    assert_eq!(pr.review_state.as_deref(), Some("changes_requested"));
}

#[cfg(unix)]
#[test]
fn gh_pr_view_terminates_options_before_a_dash_prefixed_branch() {
    // A branch name that looks like a flag (e.g. from a hostile or
    // unusual git ref) must land after `--` so `gh` reads it as the
    // positional branch argument, not an option. The fake `gh` dumps its
    // argv so the test can check `--` immediately precedes the branch.
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > argv.log\necho '{\"number\":1}'\n",
    );
    let branch = "--json";
    let _ = gh_pr_view(fake_gh(&gh.to_string_lossy()), repo_dir.path(), branch);
    let argv = std::fs::read_to_string(repo_dir.path().join("argv.log")).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    let dash_dash = args
        .iter()
        .position(|a| *a == "--")
        .expect("no -- terminator in argv");
    assert_eq!(
        args.get(dash_dash + 1),
        Some(&branch),
        "branch must immediately follow -- in argv: {args:?}"
    );
}

#[cfg(unix)]
#[test]
fn gh_pr_view_reports_a_missing_binary() {
    // Simulates "gh isn't installed": spawn itself fails (#2821).
    let repo_dir = tempfile::tempdir().unwrap();
    let missing = repo_dir.path().join("no-such-gh-binary");
    assert_eq!(
        gh_pr_view(
            fake_gh(&missing.to_string_lossy()),
            repo_dir.path(),
            "feat/x"
        ),
        Err(GhFailure::Missing)
    );
}

#[cfg(unix)]
#[test]
fn gh_pr_view_reports_a_failed_login() {
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho 'To get started with GitHub CLI, please run:  gh auth login' >&2\nexit 4\n",
    );
    assert_eq!(
        gh_pr_view(fake_gh(&gh.to_string_lossy()), repo_dir.path(), "feat/x"),
        Err(GhFailure::Unauthenticated)
    );
}

#[cfg(unix)]
#[test]
fn gh_pr_view_no_pr_when_gh_exits_nonzero_for_no_pr() {
    // "no remote" and "no open PR" are answers, so they read as no PR.
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho 'No pull requests found for branch \"feat/x\"' >&2\nexit 1\n",
    );
    assert_eq!(
        gh_pr_view(fake_gh(&gh.to_string_lossy()), repo_dir.path(), "feat/x"),
        Ok(None)
    );
}

#[cfg(unix)]
#[test]
fn gh_pr_view_fails_when_gh_outlives_its_timeout() {
    // The timeout's own contract (#1073): a `gh` that would eventually
    // print a perfectly good PR still yields None if it doesn't finish in
    // time, and gh_pr_view gives up rather than blocking the render. This
    // is the one test that legitimately asserts on the timeout, so it
    // injects a tiny budget against a fake that sleeps far longer —
    // orders of magnitude of margin, rather than racing production's 3s.
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\nsleep 30\necho '{\"number\":1}'\n",
    );
    let gh_bin = gh.to_string_lossy();
    let cmd = GhPrCmd {
        bin: &gh_bin,
        timeout: Duration::from_millis(50),
    };
    let started = std::time::Instant::now();
    assert_eq!(
        gh_pr_view(cmd, repo_dir.path(), "feat/x"),
        Err(GhFailure::Failed)
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "timeout must abandon the child, not wait it out"
    );
}

#[cfg(unix)]
#[test]
fn derive_pr_cache_miss_calls_gh_and_writes_cache() {
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho called >> gh-calls.log\necho '{\"number\":1,\"url\":null,\"reviewDecision\":null}'\n",
    );
    let pr = derive_pr(
        fake_gh(&gh.to_string_lossy()),
        repo_dir.path(),
        "feat/x",
        Some(cache_dir.path()),
        1_000,
    );
    assert_eq!(pr.clone().ok().flatten().and_then(|p| p.number), Some(1));
    let calls = std::fs::read_to_string(repo_dir.path().join("gh-calls.log")).unwrap();
    assert_eq!(calls.lines().count(), 1, "gh should be called on a miss");
    let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
    assert!(cache_path.exists(), "cache miss must write a cache entry");
}

#[cfg(unix)]
#[test]
fn classify_gh_exit_reads_only_known_answers_as_no_pr() {
    use std::os::unix::process::ExitStatusExt as _;
    let status = std::process::ExitStatus::from_raw(1 << 8);
    let no_pr = "no pull requests found for branch \"fix/authentication-retry\"";
    assert_eq!(classify_gh_exit(status, no_pr), Ok(None));
    // Real `gh` 2.102.0 capitalizes the first letter (#2975).
    let real_no_pr = "No pull requests found for branch \"release/3.x\"";
    assert_eq!(classify_gh_exit(status, real_no_pr), Ok(None));
    assert_eq!(
        classify_gh_exit(status, "To get started, please run:  gh auth login"),
        Err(GhFailure::Unauthenticated)
    );
    assert_eq!(
        classify_gh_exit(status, "error connecting to api.github.com"),
        Err(GhFailure::Failed)
    );
    assert_eq!(classify_gh_exit(status, ""), Err(GhFailure::Failed));
}

#[cfg(unix)]
proptest::proptest! {
    #[test]
    fn classify_gh_exit_never_reads_unknown_text_as_no_pr(stderr in "\\PC{0,80}") {
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let lowered = stderr.to_ascii_lowercase();
        let known = GH_NO_PR_SIGNS.iter().chain(GH_LOGIN_SIGNS.iter()).any(|s| lowered.contains(s));
        proptest::prop_assume!(!known);
        proptest::prop_assert_eq!(classify_gh_exit(status, &stderr), Err(GhFailure::Failed));
    }
}
