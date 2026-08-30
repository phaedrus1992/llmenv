#![expect(clippy::panic, reason = "test scaffolding")]
//! Guards that the `// renovate:` comment tracking `DEFAULT_SANDBOX_IMAGE`'s
//! digest pin in `src/launch/mod.rs` (#1725) stays in sync with
//! `docker/sandbox/VERSION`, and that a Renovate custom manager actually
//! targets that file. Without this, a deliberate version bump could update
//! `docker/sandbox/VERSION` and forget the renovate comment's `currentValue`,
//! silently pointing Renovate's digest checks at the wrong tag.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn read(relative: &str) -> String {
    let path = Path::new(MANIFEST_DIR).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn renovate_comment_current_value_matches_sandbox_version_file() {
    let source = read("src/launch/mod.rs");
    let comment = source
        .lines()
        .find(|line| {
            line.trim_start().starts_with("// renovate:") && line.contains("llmenv-sandbox")
        })
        .unwrap_or_else(|| {
            panic!("src/launch/mod.rs has no renovate tracking comment for the sandbox image")
        });
    let current_value = comment
        .split("currentValue=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("renovate comment has no currentValue=...: {comment}"));

    let version = read("docker/sandbox/VERSION");
    let expected = format!("v{}", version.trim());
    assert_eq!(
        current_value,
        expected,
        "renovate comment's currentValue ({current_value}) must match docker/sandbox/VERSION \
         (v{}) — update both together when cutting a new sandbox image version",
        version.trim()
    );
}

#[test]
fn renovate_config_has_a_custom_manager_targeting_the_launch_module() {
    let config = read(".github/renovate.json5");
    assert!(
        config.contains("src/launch/mod") && config.contains("customManagers"),
        ".github/renovate.json5 must define a customManagers entry tracking \
         src/launch/mod.rs's sandbox image digest pin"
    );
}
