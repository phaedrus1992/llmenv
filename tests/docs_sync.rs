#![expect(clippy::panic, reason = "test scaffolding")]
//! Guard that `website/docs/changelog.md` stays in sync with per-major-version
//! `CHANGELOG-*.md` files (#258, #673).
//!
//! `website/docs/changelog.md` is a generated artifact derived from the per-version
//! `CHANGELOG-N.md` files. The derivation is handled by `scripts/sync-changelog-doc.sh`.
//! This test replicates the transformation and asserts the committed file matches,
//! so a PR that edits a `CHANGELOG-*.md` without re-running the sync script fails CI.

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
/// strip cargo-release machine markers, strip preamble from files 2+,
/// and prepend Docusaurus frontmatter.
fn expected_site_changelog() -> String {
    let mut body_parts: Vec<String> = Vec::new();
    let mut first = true;

    // Discover CHANGELOG-N.md files and process newest-first.
    let dir = Path::new(MANIFEST_DIR);
    let mut changelogs: Vec<u32> = Vec::new();
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read changelog dir: {e}")) {
        let entry = entry.unwrap_or_else(|e| panic!("read entry: {e}"));
        let fname = entry
            .path()
            .file_name()
            .unwrap_or_else(|| panic!("entry has no filename: {:?}", entry.path()))
            .to_string_lossy()
            .to_string();
        if let Some(rest) = fname
            .strip_prefix("CHANGELOG-")
            .and_then(|s| s.strip_suffix(".md"))
            && let Ok(num) = rest.parse::<u32>()
        {
            changelogs.push(num);
        }
    }
    changelogs.sort_unstable_by(|a, b| b.cmp(a)); // newest first

    for version in changelogs {
        let fname = format!("CHANGELOG-{version}.md");
        let content = match fs::read_to_string(Path::new(MANIFEST_DIR).join(&fname)) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Drop the URL-block footer (<!-- next-url --> plus all reference links below it).
        let before_urls = if let Some(idx) = content.find("<!-- next-url -->") {
            &content[..idx]
        } else {
            &content
        };

        // Skip placeholder files with no actual changelog sections.
        if !before_urls
            .lines()
            .any(|l| l.trim_start().starts_with("## ["))
        {
            continue;
        }

        // Strip versioned sentinel lines (<!-- X.Y next-header -->).
        let stripped: Vec<&str> = before_urls
            .lines()
            .filter(|line| {
                let t = line.trim();
                !(t.starts_with("<!--") && t.ends_with("next-header -->"))
            })
            .collect();

        // Separate preamble from sections. Each file gets its own ## Version N.x
        // header so git's 3-way merge can match entries to the correct major-
        // version section independently — entries in different sections won't
        // conflict (#823).
        let preamble: Vec<&str> = stripped
            .iter()
            .copied()
            .take_while(|line| !line.trim().starts_with("## ["))
            .collect();
        let sections: Vec<&str> = stripped
            .into_iter()
            .skip_while(|line| !line.trim().starts_with("## ["))
            .collect();

        if sections.is_empty() {
            continue;
        }

        // First file keeps its preamble; files 2+ discard it.
        let preamble_str = preamble.join("\n").trim().to_string();
        if first && !preamble_str.is_empty() {
            body_parts.push(preamble_str);
        }
        first = false;

        // Every file gets a ## Version N.x section header.
        body_parts.push(format!("## Version {version}.x"));

        // Trim leading blank lines from sections, then join.
        let content = sections.join("\n").trim().to_string();
        body_parts.push(content);
    }

    let body = body_parts.join("\n\n");
    format!("{FRONTMATTER}{body}\n")
}

#[test]
fn docs_changelog_in_sync_with_root() {
    let site_changelog = read("website/docs/changelog.md");
    let expected = expected_site_changelog();

    assert_eq!(
        site_changelog, expected,
        "website/docs/changelog.md is out of sync with CHANGELOG-*.md.\n\
         Run `scripts/sync-changelog-doc.sh` and commit the result."
    );
}

#[test]
fn docs_changelog_has_no_machine_markers() {
    let site_changelog = read("website/docs/changelog.md");
    for marker in &["next-header -->", "<!-- next-url -->"] {
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
