#![expect(clippy::panic, reason = "test scaffolding")]
//! Guards that `docker/sandbox/Dockerfile`'s base image is pinned by content
//! digest, not a mutable tag (#1704). A mutable tag (`debian:bookworm-slim`)
//! can be repointed by a Debian security update or a registry-side swap with
//! no corresponding change in this repo.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn dockerfile_base_image_is_pinned_by_digest() {
    let path = Path::new(MANIFEST_DIR).join("docker/sandbox/Dockerfile");
    let dockerfile =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let from_line = dockerfile
        .lines()
        .find(|line| line.trim_start().starts_with("FROM "))
        .unwrap_or_else(|| panic!("{} has no FROM line", path.display()));
    assert!(
        from_line.contains("@sha256:"),
        "{}'s base image must pin a content digest, not a mutable tag: {from_line}",
        path.display()
    );
}
