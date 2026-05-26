use std::process::Command;

fn main() {
    // Re-run if HEAD moves (new commit, branch switch). Best-effort: missing
    // .git (e.g. crates.io tarball build) just leaves LLMENV_GIT_HASH unset
    // and the version string falls back to the bare Cargo.toml version.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Human-readable, surfaced via `llmenv --version`. Includes -dirty so
    // dev builds advertise themselves clearly.
    let version = if hash.is_empty() {
        pkg_version.clone()
    } else if dirty {
        format!("{pkg_version} ({hash}-dirty)")
    } else {
        format!("{pkg_version} ({hash})")
    };

    // Filesystem-safe prefix for cache buckets. Omits -dirty so all dev
    // iterations at a given HEAD share a bucket (avoids cache fragmentation
    // while editing). Format: `{pkg_version}-{hash}` or `{pkg_version}` when
    // no .git is present (e.g. crates.io tarball builds).
    let version_tag = if hash.is_empty() {
        pkg_version
    } else {
        format!("{pkg_version}-{hash}")
    };

    println!("cargo:rustc-env=LLMENV_VERSION={version}");
    println!("cargo:rustc-env=LLMENV_VERSION_TAG={version_tag}");
}
