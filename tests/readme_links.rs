#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Guards that README.md and Cargo.toml are fit for external publishing (#265).
//!
//! Links in a crates.io README resolve relative to the GitHub repository root.
//! The Docusaurus docs live in `website/docs/`, not `docs/`, so any link of
//! the form `docs/<page>.md` is a broken path on both GitHub and crates.io.
//! These tests enforce that all doc-page links use absolute Docusaurus URLs
//! and that Cargo.toml carries the discovery metadata crates.io surfaces.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
const DOCS_SITE: &str = "https://phaedrus1992.github.io/llmenv/";

fn read(rel: &str) -> String {
    let path = Path::new(MANIFEST_DIR).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Relative `docs/*.md` links work on a local checkout but resolve to
/// `https://github.com/<owner>/<repo>/blob/<ref>/docs/<file>.md` on crates.io
/// — a path that doesn't exist because the docs live in `website/docs/`.
#[test]
fn readme_has_no_relative_docs_links() {
    let readme = read("README.md");
    let bad: Vec<&str> = readme
        .lines()
        .filter(|line| {
            // Match markdown links like (docs/foo.md) or (docs/foo)
            line.contains("](docs/") && line.contains(".md")
        })
        .collect();
    assert!(
        bad.is_empty(),
        "README.md contains relative docs/ links that break on crates.io.\n\
         Replace with absolute Docusaurus URLs ({}*):\n{}",
        DOCS_SITE,
        bad.join("\n")
    );
}

#[test]
fn cargo_toml_has_documentation_field() {
    let manifest = read("Cargo.toml");
    assert!(
        manifest.contains("documentation ="),
        "Cargo.toml [package] is missing a `documentation` field.\n\
         Add: documentation = \"{DOCS_SITE}\""
    );
}

#[test]
fn cargo_toml_has_homepage_field() {
    let manifest = read("Cargo.toml");
    assert!(
        manifest.contains("homepage ="),
        "Cargo.toml [package] is missing a `homepage` field.\n\
         Add: homepage = \"{DOCS_SITE}\""
    );
}
