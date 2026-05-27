//! Tests for #59: `sync_marketplace` clones a git source into the shared cache
//! and reports a HEAD token, and fast-forwards on a second sync. Local *path*
//! sources are used in place (no clone, no HEAD); a `file://` URL is treated as
//! a git source and exercises the clone/refresh path.

use std::path::Path;
use std::process::Command;

use llmenv::config::Marketplace;
use llmenv::plugins::cache::sync_marketplace;
use tempfile::tempdir;

fn git(args: &[&str], dir: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Build a throwaway git repo with one commit.
fn init_source_repo(dir: &Path) {
    git(&["init", "-q"], dir);
    git(&["config", "user.email", "t@t.t"], dir);
    git(&["config", "user.name", "t"], dir);
    std::fs::write(dir.join("README.md"), "v1").expect("write");
    git(&["add", "-A"], dir);
    git(&["commit", "-q", "-m", "init"], dir);
}

/// A `file://` URL for a local repo, which `classify_source` treats as git.
fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

#[test]
fn git_source_clones_into_cache_and_reports_head() {
    let src = tempdir().expect("src");
    init_source_repo(src.path());
    let cache = tempdir().expect("cache");

    let m = Marketplace {
        name: "demo".into(),
        source: file_url(src.path()),
    };
    let state = sync_marketplace(cache.path(), &m, false).expect("sync");

    assert!(state.install_location.join(".git").exists(), "cloned repo");
    // Compare canonicalized roots — tempdir paths symlink through /private on macOS.
    let cache_root = std::fs::canonicalize(cache.path()).expect("canon cache");
    let install = std::fs::canonicalize(&state.install_location).expect("canon install");
    assert!(
        install.starts_with(&cache_root),
        "clone {install:?} lives under cache {cache_root:?}"
    );
    let head = state.head.expect("git source reports a HEAD");
    assert_eq!(head.len(), 40, "full sha");
}

#[test]
fn second_sync_with_refresh_fast_forwards_to_new_head() {
    let src = tempdir().expect("src");
    init_source_repo(src.path());
    let cache = tempdir().expect("cache");

    let m = Marketplace {
        name: "demo".into(),
        source: file_url(src.path()),
    };
    let first = sync_marketplace(cache.path(), &m, false).expect("first sync");

    std::fs::write(src.path().join("README.md"), "v2").expect("write");
    git(&["add", "-A"], src.path());
    git(&["commit", "-q", "-m", "v2"], src.path());

    let second = sync_marketplace(cache.path(), &m, true).expect("refresh sync");
    assert_ne!(
        first.head, second.head,
        "refresh should advance HEAD to the new source commit"
    );
}

#[test]
fn refresh_false_does_not_advance_existing_clone() {
    let src = tempdir().expect("src");
    init_source_repo(src.path());
    let cache = tempdir().expect("cache");

    let m = Marketplace {
        name: "demo".into(),
        source: file_url(src.path()),
    };
    let first = sync_marketplace(cache.path(), &m, false).expect("first sync");

    std::fs::write(src.path().join("README.md"), "v2").expect("write");
    git(&["add", "-A"], src.path());
    git(&["commit", "-q", "-m", "v2"], src.path());

    let second = sync_marketplace(cache.path(), &m, false).expect("no-refresh sync");
    assert_eq!(
        first.head, second.head,
        "without refresh the existing clone is reused as-is"
    );
}

#[test]
fn ext_transport_source_is_rejected() {
    // `ext::sh -c ...` runs an arbitrary command on clone; classify_source
    // treats the colon form as git, so it reaches git_clone, which must reject
    // it before spawning git.
    let cache = tempdir().expect("cache");
    let m = Marketplace {
        name: "evil".into(),
        source: "ext::sh -c id".into(),
    };
    let err = sync_marketplace(cache.path(), &m, false).expect_err("ext:: must be rejected");
    assert!(
        format!("{err:#}").contains("disallowed git transport"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn fd_transport_source_is_rejected() {
    let cache = tempdir().expect("cache");
    let m = Marketplace {
        name: "evil".into(),
        source: "fd::17".into(),
    };
    let err = sync_marketplace(cache.path(), &m, false).expect_err("fd:: must be rejected");
    assert!(
        format!("{err:#}").contains("disallowed git transport"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn local_path_source_used_in_place_without_clone() {
    let src = tempdir().expect("src");
    init_source_repo(src.path());
    let cache = tempdir().expect("cache");

    // A bare filesystem path (no scheme) is a path source: used in place.
    let m = Marketplace {
        name: "demo".into(),
        source: src.path().to_string_lossy().into_owned(),
    };
    let state = sync_marketplace(cache.path(), &m, false).expect("sync");

    assert_eq!(state.head, None, "path sources carry no HEAD token");
    assert_eq!(
        std::fs::canonicalize(&state.install_location).expect("canon"),
        std::fs::canonicalize(src.path()).expect("canon src"),
        "path source resolves to the source itself, not a cache clone"
    );
}
