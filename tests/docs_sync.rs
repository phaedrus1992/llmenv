#![expect(clippy::panic, reason = "test scaffolding")]
//! Guard that `website/docs/changelog.md` stays in sync with `CHANGELOG.md` (#258).
//!
//! `website/docs/changelog.md` is a generated artifact derived from the root
//! `CHANGELOG.md`. The derivation is handled by `scripts/sync-changelog-doc.sh`.
//! This test replicates the transformation and asserts the committed file matches,
//! so a PR that edits `CHANGELOG.md` without re-running the sync script fails CI.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn read(rel: &str) -> String {
    let path = Path::new(MANIFEST_DIR).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const FRONTMATTER: &str = "\
---
id: changelog
title: Changelog
slug: /changelog
sidebar_label: Changelog
---

{/* GENERATED FILE — do not edit by hand. Regenerate with `scripts/sync-changelog-doc.sh`. */}

";

/// Apply the same transformation that `scripts/sync-changelog-doc.sh` applies:
/// strip cargo-release machine markers and prepend Docusaurus frontmatter.
fn expected_site_changelog(changelog: &str) -> String {
    // Drop the URL-block footer (<!-- next-url --> plus all reference links below it).
    let before_urls = if let Some(idx) = changelog.find("<!-- next-url -->") {
        &changelog[..idx]
    } else {
        changelog
    };
    // Strip next-header sentinel lines (plain and scoped forms like <!-- 1.0 next-header -->).
    let stripped = before_urls
        .lines()
        .filter(|line| !line.contains("next-header"))
        .collect::<Vec<_>>()
        .join("\n");

    // Trim leading/trailing blank lines from the body, then append a final newline.
    let body = stripped.trim();
    format!("{FRONTMATTER}{body}\n")
}

#[test]
fn docs_changelog_in_sync_with_root() {
    let changelog = read("CHANGELOG.md");
    let site_changelog = read("website/docs/changelog.md");
    let expected = expected_site_changelog(&changelog);

    assert_eq!(
        site_changelog, expected,
        "website/docs/changelog.md is out of sync with CHANGELOG.md.\n\
         Run `scripts/sync-changelog-doc.sh` and commit the result."
    );
}

#[test]
fn docs_changelog_has_no_machine_markers() {
    let site_changelog = read("website/docs/changelog.md");
    for marker in &["<!-- next-header -->", "<!-- next-url -->"] {
        assert!(
            !site_changelog.contains(marker),
            "website/docs/changelog.md contains cargo-release marker `{marker}` \
             which must be stripped. Run `scripts/sync-changelog-doc.sh`."
        );
    }
}

#[test]
fn docs_changelog_has_frontmatter() {
    let site_changelog = read("website/docs/changelog.md");
    assert!(
        site_changelog.starts_with("---\n"),
        "website/docs/changelog.md must start with YAML frontmatter"
    );
    assert!(
        site_changelog.contains("title: Changelog"),
        "website/docs/changelog.md frontmatter must include `title: Changelog`"
    );
}
