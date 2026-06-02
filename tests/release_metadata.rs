#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Release-hygiene guards (#257).
//!
//! These assert invariants the cargo-release flow depends on, so a broken
//! release setup fails CI instead of surfacing at `cargo release`/publish time:
//!
//! 1. The `Cargo.toml` version (when it is a real release, not a prerelease)
//!    has a matching `## [<version>]` section in `CHANGELOG.md`. Catches the
//!    classic "bumped the crate but forgot the changelog" mistake.
//! 2. `release.toml` sets `publish = false`. crates.io publishing is owned by
//!    the `publish-crate` job in `.github/workflows/release.yml`; if
//!    cargo-release also published we would double-publish. This is the guard
//!    against re-introducing that.
//! 3. `Cargo.toml` carries `keywords` and `categories` — crates.io discovery
//!    metadata that must not silently regress to absent.
//!
//! Checks are string-based on purpose: the crate deliberately dropped the
//! `toml` dependency (#76), and re-adding a parser just for a metadata guard
//! is not worth the attack surface.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn read(rel: &str) -> String {
    let path = Path::new(MANIFEST_DIR).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Extract the `version = "..."` value from the `[package]` table. Stops at the
/// first `version` key, which in this single-crate manifest is the package one.
fn package_version(manifest: &str) -> String {
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return rest.trim().trim_matches('"').to_string();
            }
        }
    }
    panic!("no `version =` key found in Cargo.toml [package]");
}

#[test]
fn release_version_has_changelog_section() {
    let version = package_version(&read("Cargo.toml"));
    // Prereleases (1.0.0-rc1, …) are cut before the changelog section lands, so
    // only enforce the section for stable releases.
    if version.contains('-') {
        return;
    }
    let changelog = read("CHANGELOG.md");
    let heading = format!("## [{version}]");
    assert!(
        changelog.contains(&heading),
        "CHANGELOG.md is missing a `{heading}` section for the current crate version"
    );
}

#[test]
fn release_toml_disables_cargo_release_publish() {
    let release = read("release.toml");
    let has_publish_false = release
        .lines()
        .map(str::trim)
        .any(|l| l.starts_with("publish") && l.contains("false"));
    assert!(
        has_publish_false,
        "release.toml must set `publish = false`; crates.io publishing is owned \
         by the publish-crate job in release.yml, and cargo-release publishing \
         too would double-publish"
    );
}

#[test]
fn manifest_has_crates_io_discovery_metadata() {
    let manifest = read("Cargo.toml");
    for key in ["keywords", "categories"] {
        let present = manifest
            .lines()
            .map(str::trim)
            .any(|l| l.starts_with(key) && l.contains('['));
        assert!(
            present,
            "Cargo.toml is missing a `{key}` array for crates.io"
        );
    }
}
