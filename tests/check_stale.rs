#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Tests for the `llmenv check-stale` subcommand (#121 / #85 gap).
//!
//! The SessionStart hook emitted by the Claude Code adapter invokes
//! `llmenv check-stale`. That command compares the *booted* content hash —
//! read from the `.llmenv-manifest.json` dotfile in the booted folder
//! (`CLAUDE_CONFIG_DIR`) — against the hash llmenv would render now (#196).
//! When they diverge, the environment drifted after the agent booted (config
//! edited, scope changed, bundle toggled, plugin path moved) and the user
//! should restart the session to pick it up. The comparison is hash-based in
//! both hashing modes, since a version-mode folder name is stable across edits.

use llmenv::cli::{StaleStatus, stale_status};
use proptest::prelude::*;

#[test]
fn fresh_when_booted_matches_current() {
    // Booted content hash equals the freshly-computed hash → no drift.
    let status = stale_status(Some("abc123"), "abc123");
    assert_eq!(status, StaleStatus::Fresh);
}

#[test]
fn stale_when_booted_differs_from_current() {
    // Config drifted since boot: the hash the agent booted with no longer
    // matches what llmenv would materialize now.
    let status = stale_status(Some("abc123"), "def456");
    assert_eq!(
        status,
        StaleStatus::Stale {
            booted: "abc123".to_string(),
            current: "def456".to_string(),
        }
    );
}

#[test]
fn unknown_when_no_booted_dir() {
    // CLAUDE_CONFIG_DIR unset, or the booted folder has no manifest dotfile →
    // no baseline to compare. This is not "stale" (don't nag); it just means
    // llmenv didn't boot this agent.
    let status = stale_status(None, "def456");
    assert_eq!(status, StaleStatus::Unknown);
}

#[test]
fn stale_status_is_drift_only_on_stale() {
    assert!(!StaleStatus::Fresh.is_drift());
    assert!(!StaleStatus::Unknown.is_drift());
    assert!(
        StaleStatus::Stale {
            booted: "a".into(),
            current: "b".into(),
        }
        .is_drift()
    );
}

proptest! {
    /// Equal booted/current → always Fresh, never drift, for any string
    /// (including empty, whitespace, unicode).
    #[test]
    fn equal_hashes_are_always_fresh(s in ".*") {
        let status = stale_status(Some(&s), &s);
        prop_assert_eq!(&status, &StaleStatus::Fresh);
        prop_assert!(!status.is_drift());
    }

    /// `is_drift()` is true iff the status is Stale, which happens iff a booted
    /// hash is present AND differs from current. This ties the classification
    /// and the drift predicate together so they can't disagree.
    #[test]
    fn drift_iff_present_and_differing(booted in proptest::option::of(".*"), current in ".*") {
        let status = stale_status(booted.as_deref(), &current);
        let expect_drift = matches!(&booted, Some(b) if *b != current);
        prop_assert_eq!(status.is_drift(), expect_drift);
        match (&booted, &status) {
            (None, s) => prop_assert_eq!(s, &StaleStatus::Unknown),
            (Some(b), _) if *b == current => prop_assert_eq!(&status, &StaleStatus::Fresh),
            (Some(b), StaleStatus::Stale { booted: rb, current: rc }) => {
                prop_assert_eq!(rb, b);
                prop_assert_eq!(rc, &current);
            }
            (Some(_), other) => prop_assert!(false, "expected Stale, got {other:?}"),
        }
    }
}
