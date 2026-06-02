#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests for #59: `sync_marketplace` clones a git source into the shared cache
//! and reports a HEAD token, and fast-forwards on a second sync. Local *path*
//! sources are used in place (no clone, no HEAD); the `ext::`/`fd::` transports
//! are rejected before any clone is attempted.
//!
//! These exercise the clone/pull/head *sequencing*, not the real `git` binary,
//! so they inject a `FakeGit` backend. That keeps the suite fast, hermetic, and
//! free of network / git-identity / credential-prompt flakiness. The real
//! `SystemGit` impl is a thin wrapper over the same free functions and is
//! covered by the unit tests in `src/plugins/cache.rs`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use llmenv::config::Marketplace;
use llmenv::plugins::cache::{GitBackend, sync_marketplace_with};
use tempfile::tempdir;

/// A git backend that simulates clone/pull/head without spawning `git`.
///
/// `source_head` is the HEAD the simulated remote currently points at; advance
/// it to model an upstream commit. A "clone" creates `<dest>/.git` and records
/// the dest's current head as the source head at clone time. `pull` advances a
/// clone's head to the current source head; `head` reads whatever head the
/// clone last recorded.
struct FakeGit {
    source_head: RefCell<String>,
    // Per-clone recorded head, keyed by dest path.
    cloned: RefCell<std::collections::HashMap<PathBuf, String>>,
    clone_calls: RefCell<usize>,
    pull_calls: RefCell<usize>,
}

impl FakeGit {
    fn new(initial_head: &str) -> Self {
        Self {
            source_head: RefCell::new(initial_head.to_string()),
            cloned: RefCell::new(std::collections::HashMap::new()),
            clone_calls: RefCell::new(0),
            pull_calls: RefCell::new(0),
        }
    }

    fn advance_source(&self, new_head: &str) {
        *self.source_head.borrow_mut() = new_head.to_string();
    }
}

impl GitBackend for FakeGit {
    fn clone(&self, _source: &str, dest: &Path) -> anyhow::Result<()> {
        *self.clone_calls.borrow_mut() += 1;
        std::fs::create_dir_all(dest.join(".git"))?;
        self.cloned
            .borrow_mut()
            .insert(dest.to_path_buf(), self.source_head.borrow().clone());
        Ok(())
    }

    fn pull(&self, repo: &Path) -> anyhow::Result<()> {
        *self.pull_calls.borrow_mut() += 1;
        self.cloned
            .borrow_mut()
            .insert(repo.to_path_buf(), self.source_head.borrow().clone());
        Ok(())
    }

    fn head(&self, repo: &Path) -> Option<String> {
        self.cloned.borrow().get(repo).cloned()
    }
}

#[test]
fn git_source_clones_into_cache_and_reports_head() {
    let cache = tempdir().expect("cache");
    let git = FakeGit::new("a".repeat(40).as_str());

    let m = Marketplace {
        name: "demo".into(),
        source: "https://example.com/demo.git".into(),
    };
    let state = sync_marketplace_with(cache.path(), &m, true, &git).expect("sync");

    assert!(state.install_location.join(".git").exists(), "cloned repo");
    let cache_root = std::fs::canonicalize(cache.path()).expect("canon cache");
    let install = std::fs::canonicalize(&state.install_location).expect("canon install");
    assert!(
        install.starts_with(&cache_root),
        "clone {install:?} lives under cache {cache_root:?}"
    );
    let head = state.head.expect("git source reports a HEAD");
    assert_eq!(head.len(), 40, "full sha");
    assert_eq!(*git.clone_calls.borrow(), 1, "cloned exactly once");
}

#[test]
fn second_sync_with_refresh_fast_forwards_to_new_head() {
    let cache = tempdir().expect("cache");
    let git = FakeGit::new("a".repeat(40).as_str());

    let m = Marketplace {
        name: "demo".into(),
        source: "https://example.com/demo.git".into(),
    };
    let first = sync_marketplace_with(cache.path(), &m, true, &git).expect("first sync");

    git.advance_source("b".repeat(40).as_str());

    let second = sync_marketplace_with(cache.path(), &m, true, &git).expect("refresh sync");
    assert_ne!(
        first.head, second.head,
        "refresh should advance HEAD to the new source commit"
    );
    assert_eq!(*git.clone_calls.borrow(), 1, "no re-clone on refresh");
    assert_eq!(*git.pull_calls.borrow(), 1, "refresh pulls once");
}

#[test]
fn refresh_false_does_not_advance_existing_clone() {
    let cache = tempdir().expect("cache");
    let git = FakeGit::new("a".repeat(40).as_str());

    let m = Marketplace {
        name: "demo".into(),
        source: "https://example.com/demo.git".into(),
    };
    let first = sync_marketplace_with(cache.path(), &m, true, &git).expect("first sync");

    git.advance_source("b".repeat(40).as_str());

    let second = sync_marketplace_with(cache.path(), &m, false, &git).expect("no-refresh sync");
    assert_eq!(
        first.head, second.head,
        "without refresh the existing clone is reused as-is"
    );
    assert_eq!(*git.pull_calls.borrow(), 0, "no-refresh never pulls");
}

#[test]
fn ext_transport_source_is_rejected() {
    // `ext::sh -c ...` runs an arbitrary command on clone; classify_source
    // treats the colon form as git, so it reaches sync_git, which must reject
    // it before invoking the backend.
    let cache = tempdir().expect("cache");
    let git = FakeGit::new("a".repeat(40).as_str());
    let m = Marketplace {
        name: "evil".into(),
        source: "ext::sh -c id".into(),
    };
    let err =
        sync_marketplace_with(cache.path(), &m, false, &git).expect_err("ext:: must be rejected");
    assert!(
        format!("{err:#}").contains("disallowed git transport"),
        "unexpected error: {err:#}"
    );
    assert_eq!(*git.clone_calls.borrow(), 0, "backend never invoked");
}

#[test]
fn fd_transport_source_is_rejected() {
    let cache = tempdir().expect("cache");
    let git = FakeGit::new("a".repeat(40).as_str());
    let m = Marketplace {
        name: "evil".into(),
        source: "fd::17".into(),
    };
    let err =
        sync_marketplace_with(cache.path(), &m, false, &git).expect_err("fd:: must be rejected");
    assert!(
        format!("{err:#}").contains("disallowed git transport"),
        "unexpected error: {err:#}"
    );
    assert_eq!(*git.clone_calls.borrow(), 0, "backend never invoked");
}

#[test]
fn git_clone_failure_with_refresh_true_propagates() {
    struct FailClone;
    impl GitBackend for FailClone {
        fn clone(&self, _: &str, _: &std::path::Path) -> anyhow::Result<()> {
            anyhow::bail!("simulated clone failure")
        }
        fn pull(&self, _: &std::path::Path) -> anyhow::Result<()> {
            unreachable!()
        }
        fn head(&self, _: &std::path::Path) -> Option<String> {
            None
        }
    }

    let cache = tempdir().expect("cache");
    let m = Marketplace {
        name: "broken".into(),
        source: "https://example.com/broken.git".into(),
    };
    let err = sync_marketplace_with(cache.path(), &m, true, &FailClone)
        .expect_err("clone failure with refresh=true must propagate");
    assert!(
        format!("{err:#}").contains("simulated clone failure"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn local_path_source_used_in_place_without_clone() {
    let src = tempdir().expect("src");
    let cache = tempdir().expect("cache");
    let git = FakeGit::new("a".repeat(40).as_str());

    // A bare filesystem path (no scheme) is a path source: used in place.
    let m = Marketplace {
        name: "demo".into(),
        source: src.path().to_string_lossy().into_owned(),
    };
    let state = sync_marketplace_with(cache.path(), &m, false, &git).expect("sync");

    assert_eq!(state.head, None, "path sources carry no HEAD token");
    assert_eq!(
        std::fs::canonicalize(&state.install_location).expect("canon"),
        std::fs::canonicalize(src.path()).expect("canon src"),
        "path source resolves to the source itself, not a cache clone"
    );
    assert_eq!(*git.clone_calls.borrow(), 0, "path source never clones");
}
