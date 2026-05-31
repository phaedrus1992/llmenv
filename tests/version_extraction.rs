#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Unit tests for the version-tag extraction used by doctor's version-skew
//! detection (#196).
//!
//! A cache folder is either strict-mode (`<version_tag>-<content_hash>`, where
//! the hash is exactly 64 lowercase hex digits) or version-mode (a bare version
//! like `1.2` or `1.2.3-rc.1`, no hash suffix). Recovering the version means
//! stripping a trailing `-<hash>` *only* when the tail is a real content hash —
//! otherwise the whole name is the version. This mirrors the guarded logic in
//! `run_doctor` (src/cli/mod.rs), which gates the `rsplit_once('-')` split on
//! `is_content_hash(tail)`. Splitting on the rightmost dash unconditionally
//! would corrupt semver prerelease names like `1.2.3-rc.1`.

/// True if `s` is exactly 64 lowercase hex digits — the content-hash shape.
/// Kept in lockstep with `is_content_hash` in src/cli/mod.rs.
fn is_content_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Strip a trailing `-<content_hash>` to recover the version tag, or return the
/// whole name when there is no hash suffix (version-mode folder).
fn extract_version_from_dir_name(dir_name: &str) -> &str {
    match dir_name.rsplit_once('-') {
        Some((version, tail)) if is_content_hash(tail) => version,
        _ => dir_name,
    }
}

const HASH: &str = "e8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7c8c7";

#[test]
fn strict_folder_strips_content_hash() {
    // <version>-<64-hex>: the hash suffix is stripped to recover the version.
    let dir = format!("1.2.3-{HASH}");
    assert_eq!(extract_version_from_dir_name(&dir), "1.2.3");
}

#[test]
fn strict_folder_with_prerelease_preserves_dashes_in_version() {
    // The version itself contains dashes (semver prerelease). Only the trailing
    // content hash is removed; the prerelease dashes are preserved.
    let dir = format!("1.2.3-rc.1-{HASH}");
    assert_eq!(extract_version_from_dir_name(&dir), "1.2.3-rc.1");
}

#[test]
fn version_mode_folder_has_no_hash_suffix() {
    // A bare version-mode folder: no content-hash tail, so nothing is stripped.
    assert_eq!(extract_version_from_dir_name("1.2"), "1.2");
    assert_eq!(extract_version_from_dir_name("1.2.3"), "1.2.3");
    assert_eq!(extract_version_from_dir_name("1.2.3-rc.1"), "1.2.3-rc.1");
}

#[test]
fn short_dashed_suffix_is_not_a_content_hash() {
    // A short suffix like `abc123` is NOT a 64-hex hash, so the name is treated
    // as a (whole) version, not split. The old unguarded helper wrongly split
    // here — this is the bug the guard prevents.
    assert_eq!(
        extract_version_from_dir_name("1.2.3-abc123"),
        "1.2.3-abc123"
    );
}

#[test]
fn uppercase_hex_tail_is_not_a_content_hash() {
    // Content hashes are lowercase; an uppercase 64-char tail is rejected.
    let upper = HASH.to_uppercase();
    let dir = format!("1.2.3-{upper}");
    assert_eq!(extract_version_from_dir_name(&dir), dir);
}

#[test]
fn version_skew_detected_when_versions_differ() {
    let running = "1.2.3";
    let dir_a = format!("1.2.4-{HASH}");
    let dir_b = format!("1.2.2-{HASH}");
    let cached = [
        extract_version_from_dir_name(&dir_a),
        extract_version_from_dir_name(&dir_b),
    ];
    assert!(
        !cached.contains(&running),
        "version skew should be detected"
    );
}

#[test]
fn version_skew_prerelease_vs_release_are_distinct() {
    let running = "1.2.3-rc.1";
    let dir = format!("1.2.3-{HASH}");
    let cached = extract_version_from_dir_name(&dir);
    assert_ne!(
        cached, running,
        "prerelease and release are different versions"
    );
}

#[test]
fn is_content_hash_matches_only_64_lowercase_hex() {
    assert!(is_content_hash(HASH));
    assert!(!is_content_hash("abc123"));
    assert!(!is_content_hash(&HASH[..63])); // too short
    assert!(!is_content_hash(&HASH.to_uppercase())); // uppercase
    assert!(!is_content_hash(&format!("{}g", &HASH[..63]))); // non-hex char
}
