#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
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
//! 4. `release.toml`'s `tag-name` prefix stays in sync with the `v*` tag
//!    trigger in `release.yml`. The release is tag-triggered, so a drift here
//!    would silently stop releases from firing.
//!
//! Checks are string-based on purpose: the crate deliberately dropped the
//! `toml` dependency (#76), and re-adding a parser just for a metadata guard
//! is not worth the attack surface. The helpers below are deliberately narrow
//! — they scope to the relevant table, skip comments/blanks, and require real
//! `key = value` structure — so the guards don't pass on coincidental matches.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn read(rel: &str) -> String {
    let path = Path::new(MANIFEST_DIR).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Content lines belonging to the top-level `[section]` table: from its header
/// until the next `[`-prefixed header, with comment and blank lines stripped.
/// `[package.metadata]` and the like do not match `[package]` (exact header).
fn table_lines<'a>(toml: &'a str, section: &str) -> Vec<&'a str> {
    let header = format!("[{section}]");
    let mut in_section = false;
    let mut lines = Vec::new();
    for raw in toml.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_section = line == header;
            continue;
        }
        if in_section {
            lines.push(line);
        }
    }
    lines
}

/// Value of a `key = value` pair from `line`, or `None` if `line` is not that
/// key. Requires the key to be followed (after optional whitespace) by `=`, so
/// `versioning = …` does not match key `version`. Strips surrounding quotes and
/// any trailing ` # comment`.
fn key_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?;
    let value = rest.split('#').next().unwrap_or("").trim();
    Some(value.trim_matches('"'))
}

/// Extract the `version` value from the `[package]` table.
fn package_version(manifest: &str) -> String {
    for line in table_lines(manifest, "package") {
        if let Some(value) = key_value(line, "version") {
            return value.to_string();
        }
    }
    panic!("no `version =` key found in Cargo.toml [package]");
}

/// Value of a top-level `key` in a TOML file (keys above the first `[section]`
/// header). `release.toml` is flat, so all its scalar keys live here.
fn top_level_value(toml: &str, key: &str) -> Option<String> {
    for raw in toml.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            break;
        }
        if let Some(value) = key_value(line, key) {
            return Some(value.to_string());
        }
    }
    None
}

#[test]
fn release_version_has_changelog_section() {
    let version = package_version(&read("Cargo.toml"));
    // Prereleases (1.0.0-rc1, …) are cut before the changelog section lands, so
    // only enforce the section for stable releases.
    if version.contains('-') {
        return;
    }
    let heading = format!("## [{version}]");
    let mut found = false;
    for version_file in &[
        "CHANGELOG-1.md",
        "CHANGELOG-2.md",
        "CHANGELOG-3.md",
        "CHANGELOG-4.md",
    ] {
        if let Ok(changelog) = fs::read_to_string(Path::new(MANIFEST_DIR).join(version_file))
            && changelog.contains(&heading)
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "No CHANGELOG-*.md has a `{heading}` section for the current crate version"
    );
}

#[test]
fn release_toml_disables_cargo_release_publish() {
    let publish = top_level_value(&read("release.toml"), "publish");
    assert_eq!(
        publish.as_deref(),
        Some("false"),
        "release.toml must set `publish = false`; crates.io publishing is owned \
         by the publish-crate job in release.yml, and cargo-release publishing \
         too would double-publish"
    );
}

#[test]
fn manifest_has_crates_io_discovery_metadata() {
    let manifest = read("Cargo.toml");
    let package = table_lines(&manifest, "package");
    for key in ["keywords", "categories"] {
        let present = package
            .iter()
            .filter_map(|line| key_value(line, key))
            .any(|value| value.starts_with('['));
        assert!(
            present,
            "Cargo.toml [package] is missing a `{key}` array for crates.io discovery"
        );
    }
}

#[test]
fn release_toml_tag_name_matches_ci_trigger() {
    // The release is tag-triggered: pushing a `v*` tag fires release.yml.
    // cargo-release names the tag from `tag-name`, so if that prefix ever drifts
    // from the CI `v*` filter, releases would silently stop firing. Guard the
    // coupling from both sides.
    let tag_name =
        top_level_value(&read("release.toml"), "tag-name").expect("release.toml must set tag-name");
    assert!(
        tag_name.starts_with('v'),
        "release.toml tag-name `{tag_name}` must start with `v` to match the `v*` \
         tag trigger in .github/workflows/release.yml"
    );

    let workflow = read(".github/workflows/release.yml");
    assert!(
        workflow.contains("v*"),
        ".github/workflows/release.yml must trigger on `v*` tags to match \
         release.toml tag-name `{tag_name}`"
    );
}
