#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Unit tests for version tag extraction logic

// Test the version extraction logic used in doctor's version skew detection
fn extract_version_from_dir_name(dir_name: &str) -> Option<&str> {
    // This mirrors the logic in src/cli/mod.rs:276
    // Use rsplit_once to split from right to preserve any dashes in semver prerelease versions
    dir_name.rsplit_once('-').map(|(v, _)| v)
}

#[test]
fn test_extract_version_simple_semver() {
    // Basic semantic version with hash
    let result = extract_version_from_dir_name("1.2.3-abc123");
    assert_eq!(result, Some("1.2.3"));
}

#[test]
fn test_extract_version_with_prerelease() {
    // Semver prerelease version (includes dash in version itself)
    let result = extract_version_from_dir_name("1.2.3-rc.1-abc123");
    assert_eq!(
        result,
        Some("1.2.3-rc.1"),
        "should preserve dashes in version tag"
    );
}

#[test]
fn test_extract_version_with_multiple_dashes() {
    // Complex prerelease like "1.2.3-beta-2-hash"
    let result = extract_version_from_dir_name("1.2.3-beta-2-hash");
    assert_eq!(
        result,
        Some("1.2.3-beta-2"),
        "should split from rightmost dash only"
    );
}

#[test]
fn test_extract_version_no_hash() {
    // Version without hash (edge case)
    let result = extract_version_from_dir_name("1.2.3");
    assert_eq!(result, None, "no dash means no extraction");
}

#[test]
fn test_extract_version_hash_only() {
    // Just a hash (malformed)
    let result = extract_version_from_dir_name("-abc123");
    assert_eq!(result, Some(""), "empty version before dash");
}

#[test]
fn test_extract_version_long_hash() {
    // Realistic full git hash
    let result = extract_version_from_dir_name("1.2.3-e8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c");
    assert_eq!(result, Some("1.2.3"));
}

#[test]
fn test_version_comparison_matching_versions() {
    // Simulate version comparison logic from doctor
    let running_version = "1.2.3";
    let cached_versions = ["1.2.3"];

    let has_match = cached_versions.iter().any(|v| v == &running_version);
    assert!(has_match, "matching versions should be detected");
}

#[test]
fn test_version_comparison_with_prerelease() {
    // Prerelease version comparison
    let running_version = "1.2.3-rc.1";
    let extracted_from_cache = extract_version_from_dir_name("1.2.3-rc.1-hash").unwrap();

    assert_eq!(
        extracted_from_cache, running_version,
        "extracted prerelease should match running version"
    );
}

#[test]
fn test_version_skew_detection_different_versions() {
    // Ensure version skew is detected when versions differ
    let running_version = "1.2.3";
    let cached_versions = [
        extract_version_from_dir_name("1.2.4-hash").unwrap(),
        extract_version_from_dir_name("1.2.2-hash").unwrap(),
    ];

    let has_match = cached_versions.iter().any(|v| v == &running_version);
    assert!(!has_match, "version skew should be detected");
}

#[test]
fn test_version_skew_detection_prerelease_vs_release() {
    // Prerelease should NOT match corresponding release
    let running_version = "1.2.3-rc.1";
    let cached_version = extract_version_from_dir_name("1.2.3-hash").unwrap();

    assert_ne!(
        cached_version, running_version,
        "prerelease and release should be treated as different versions"
    );
}
