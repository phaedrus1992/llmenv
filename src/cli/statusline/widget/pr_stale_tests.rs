//! Tests of the stale-PR rules of the `pr` widget cache (#3889): the last PR stays when a lookup
//! fails, a failure expires sooner than an answer, and the widget shows the cause and the grey.

#![expect(clippy::unwrap_used, reason = "test code")]

use super::gh_tests::{fake_gh, write_fake_gh};
use super::pr_cache::PrCacheEntry;
use super::pr_cache::{PR_CACHE_TTL_SECS, PR_FAILURE_TTL_SECS, STALE_PR_MAX_AGE_SECS};
use super::*;
use proptest::prelude::*;

/// A `gh` that answers with PR number 42, unless a `fail` file or a `none` file is in the folder
/// it runs in. Each run adds a line to `gh-calls.log`.
#[cfg(unix)]
const SWITCH_GH: &str = r#"#!/bin/sh
echo called >> gh-calls.log
if [ -f fail ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi
if [ -f none ]; then echo 'no pull requests found for branch "x"' >&2; exit 1; fi
echo '{"number":42,"url":null,"reviewDecision":"APPROVED"}'
"#;

#[cfg(unix)]
struct Fixture {
    _bin: tempfile::TempDir,
    repo: tempfile::TempDir,
    cache: tempfile::TempDir,
    gh: PathBuf,
}

#[cfg(unix)]
impl Fixture {
    fn new() -> Self {
        let bin = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(bin.path(), SWITCH_GH);
        Self {
            _bin: bin,
            repo: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
            gh,
        }
    }

    fn derive(&self, now: i64) -> (PrLookup, bool) {
        let gh = self.gh.to_string_lossy();
        derive_pr(
            fake_gh(&gh),
            self.repo.path(),
            "feat/x",
            Some(self.cache.path()),
            now,
        )
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(self.repo.path().join("gh-calls.log"))
            .map_or(0, |text| text.lines().count())
    }

    fn set(&self, flag: &str, on: bool) {
        let path = self.repo.path().join(flag);
        if on {
            std::fs::write(path, "").unwrap();
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn number(answer: &(PrLookup, bool)) -> Option<u64> {
    answer.0.clone().ok().flatten().and_then(|pr| pr.number)
}

#[cfg(unix)]
#[test]
fn a_failed_refresh_keeps_the_last_pr_and_marks_it_stale() {
    let fx = Fixture::new();
    assert_eq!(fx.derive(1_000), (Ok(Some(pr_42())), false));
    fx.set("fail", true);
    let answer = fx.derive(1_000 + PR_CACHE_TTL_SECS + 1);
    assert_eq!(number(&answer), Some(42));
    assert!(answer.1, "a kept PR shown after a failure is stale");
    assert_eq!(fx.calls(), 2);
}

#[cfg(unix)]
#[test]
fn a_fresh_failure_still_shows_the_kept_pr() {
    let fx = Fixture::new();
    let _answer = fx.derive(1_000);
    fx.set("fail", true);
    let _answer = fx.derive(2_000);
    let again = fx.derive(2_000 + PR_FAILURE_TTL_SECS - 1);
    assert_eq!(number(&again), Some(42));
    assert!(again.1);
    assert_eq!(fx.calls(), 2, "a fresh failure is not looked up again");
}

#[cfg(unix)]
#[test]
fn a_failure_is_looked_up_again_after_its_short_ttl_and_an_answer_after_its_own() {
    let fx = Fixture::new();
    fx.set("fail", true);
    assert_eq!(fx.derive(1_000).0, Err(GhFailure::Failed));
    let _answer = fx.derive(1_000 + PR_FAILURE_TTL_SECS - 1);
    assert_eq!(fx.calls(), 1, "the failure is reused inside its TTL");
    let _answer = fx.derive(1_000 + PR_FAILURE_TTL_SECS);
    assert_eq!(
        fx.calls(),
        2,
        "the failure expires after its TTL, not the TTL of an answer"
    );
}

#[cfg(unix)]
#[test]
fn an_answer_inside_its_ttl_is_not_stale() {
    let fx = Fixture::new();
    let _answer = fx.derive(1_000);
    let answer = fx.derive(1_000 + PR_CACHE_TTL_SECS - 1);
    assert_eq!(answer, (Ok(Some(pr_42())), false));
    assert_eq!(fx.calls(), 1);
}

#[cfg(unix)]
#[test]
fn a_failure_with_no_kept_pr_shows_the_failure() {
    let fx = Fixture::new();
    fx.set("fail", true);
    assert_eq!(fx.derive(1_000), (Err(GhFailure::Failed), false));
}

#[cfg(unix)]
#[test]
fn a_kept_pr_older_than_a_day_is_not_shown() {
    let fx = Fixture::new();
    let _answer = fx.derive(1_000);
    fx.set("fail", true);
    let answer = fx.derive(1_000 + STALE_PR_MAX_AGE_SECS);
    assert_eq!(answer, (Err(GhFailure::Failed), false));
}

#[cfg(unix)]
#[test]
fn a_lookup_with_no_pr_clears_the_kept_pr() {
    let fx = Fixture::new();
    let _answer = fx.derive(1_000);
    fx.set("none", true);
    assert_eq!(fx.derive(1_100), (Ok(None), false));
    fx.set("none", false);
    fx.set("fail", true);
    // The PR was closed, so a later failure does not bring it back.
    assert_eq!(fx.derive(1_200), (Err(GhFailure::Failed), false));
}

/// An answer that is due and older than a day is not shown as the current PR, while the refresh
/// runs (#3889).
#[cfg(unix)]
#[test]
fn a_due_answer_older_than_a_day_is_not_shown() {
    let fx = Fixture::new();
    let _answer = fx.derive(1_000);
    let gh = fx.gh.to_string_lossy();
    let mut cmd = fake_gh(&gh);
    cmd.background = true;
    let long_after = 1_000 + STALE_PR_MAX_AGE_SECS + 1;
    let answer = derive_pr(
        cmd,
        fx.repo.path(),
        "feat/x",
        Some(fx.cache.path()),
        long_after,
    );
    assert_eq!(answer, (Ok(None), false));
}

/// An entry with a time in the future is old, so a clock that stepped back keeps nothing alive.
#[cfg(unix)]
#[test]
fn an_entry_from_the_future_is_not_fresh() {
    let fx = Fixture::new();
    let _answer = fx.derive(5_000);
    let answer = fx.derive(1_000);
    assert_eq!(fx.calls(), 2, "an entry from the future is looked up again");
    assert_eq!(number(&answer), Some(42));
}

/// A refresh that cannot start shows as a failure when there is nothing cached, so it does not
/// look like "no PR".
#[cfg(unix)]
#[test]
fn a_refresh_that_cannot_start_shows_a_failure_when_nothing_is_cached() {
    let fx = Fixture::new();
    // A file where the cache folder should be.
    let blocker = fx.repo.path().join("not-a-folder");
    std::fs::write(&blocker, "").unwrap();
    let gh = fx.gh.to_string_lossy();
    let mut cmd = fake_gh(&gh);
    cmd.background = true;
    let answer = derive_pr(
        cmd,
        fx.repo.path(),
        "feat/x",
        Some(&blocker.join("cache")),
        1_000,
    );
    assert_eq!(answer, (Err(GhFailure::Failed), false));
}

#[cfg(unix)]
#[test]
fn a_detached_refresh_shows_what_the_cache_holds_and_does_not_run_gh() {
    let fx = Fixture::new();
    let _answer = fx.derive(1_000);
    let calls = fx.calls();
    let gh = fx.gh.to_string_lossy();
    let mut cmd = fake_gh(&gh);
    cmd.background = true;
    let due = 1_000 + PR_CACHE_TTL_SECS + 1;
    let answer = derive_pr(cmd, fx.repo.path(), "feat/x", Some(fx.cache.path()), due);
    assert_eq!(number(&answer), Some(42));
    assert!(answer.1, "a due entry shown while it refreshes is stale");
    assert_eq!(fx.calls(), calls, "the render does not run gh");
    // A branch with no cache entry has nothing to show yet.
    let other = derive_pr(
        cmd,
        fx.repo.path(),
        "feat/other",
        Some(fx.cache.path()),
        due,
    );
    assert_eq!(other, (Ok(None), false));
    assert_eq!(fx.calls(), calls);
}

#[cfg(unix)]
fn pr_42() -> PrInfo {
    PrInfo {
        number: Some(42),
        url: None,
        review_state: Some("approved".to_string()),
    }
}

fn view(number: u64, stale: bool) -> Result<(Option<PrInfo>, bool), PrError> {
    Ok((
        Some(PrInfo {
            number: Some(number),
            url: None,
            review_state: Some("approved".to_string()),
        }),
        stale,
    ))
}

#[test]
fn a_stale_pr_shows_in_grey_and_a_live_one_in_its_review_color() {
    let stale = render_pr_widget(view(7, true), None, true);
    assert_eq!(stale, "\x1b[38;5;248m#7\x1b[0m");
    let live = render_pr_widget(view(7, false), None, true);
    assert!(
        live.contains("\x1b[32m"),
        "an approved PR is green: {live:?}"
    );
    assert!(!live.contains("248"), "{live:?}");
}

#[test]
fn a_failure_shows_the_marker_and_the_cause() {
    for (failure, cause) in [
        (GhFailure::Unauthenticated, "auth"),
        (GhFailure::TimedOut, "timeout"),
        (GhFailure::Missing, "no gh"),
        (GhFailure::Failed, "failed"),
    ] {
        let out = render_pr_widget(Err(PrError::Gh(failure)), None, false);
        assert_eq!(out, format!("#? {cause}"));
    }
    let branch = render_pr_widget(Err(PrError::Branch("x".into())), None, false);
    assert_eq!(branch, "#?", "a branch error has no gh cause");
}

fn arb_entry() -> impl Strategy<Value = PrCacheEntry> {
    let pr = || PrInfo {
        number: Some(1),
        url: None,
        review_state: None,
    };
    (
        0i64..2_000_000,
        any::<bool>(),
        proptest::option::of(prop_oneof![
            Just(GhFailure::Unauthenticated),
            Just(GhFailure::TimedOut),
            Just(GhFailure::Failed),
        ]),
        proptest::option::of(0i64..2_000_000),
    )
        .prop_map(move |(ts, has_pr, failure, good_ts)| PrCacheEntry {
            ts,
            pr: (has_pr && failure.is_none()).then(pr),
            failure,
            good: good_ts.map(|t| (t, pr())),
        })
}

proptest! {
    // The kept PR shows only while it is younger than a day and not from the future, and a stale
    // answer always carries that PR (#3889).
    #[test]
    fn a_stale_answer_is_the_kept_pr_and_never_older_than_a_day(
        entry in arb_entry(),
        now in 0i64..2_000_000,
        fresh in any::<bool>(),
    ) {
        let (lookup, stale) = entry.show(now, fresh);
        if stale {
            let (ts, pr) = entry.good.clone().unwrap();
            prop_assert!((0..STALE_PR_MAX_AGE_SECS).contains(&(now - ts)));
            prop_assert_eq!(lookup.clone(), Ok(Some(pr)));
        }
        // A fresh answer that is not a failure shows as it is.
        if fresh && entry.failure.is_none() {
            prop_assert_eq!(entry.show(now, true), (entry.lookup(), false));
        }
        // A failure with no kept PR shows the failure.
        if !stale && let Some(failure) = entry.failure {
            prop_assert_eq!(lookup, Err(failure));
        }
    }

    // An entry is fresh only inside its own TTL, and an entry from the future is not fresh.
    #[test]
    fn an_entry_is_fresh_only_inside_its_ttl_and_never_from_the_future(
        entry in arb_entry(),
        now in 0i64..2_000_000,
    ) {
        let ttl = if entry.failure.is_some() { PR_FAILURE_TTL_SECS } else { PR_CACHE_TTL_SECS };
        prop_assert_eq!(entry.is_fresh(now), (0..ttl).contains(&(now - entry.ts)));
    }
}
