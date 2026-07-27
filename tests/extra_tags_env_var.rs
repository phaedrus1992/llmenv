#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! End-to-end coverage for `$LLMENV_EXTRA_TAGS` (#1020): verifies the env var
//! is actually wired up in the running binary, not just in `parse_extra_tags`
//! and `evaluate()` unit tests — a typo in the variable name read by
//! `Env::detect_fresh` would leave those unit tests green while the feature
//! is dead.

use std::process::Command;
use tempfile::TempDir;

#[test]
fn export_includes_llmenv_extra_tags() {
    let temp = TempDir::new().expect("create temp dir");
    let config_dir = temp.path().to_str().expect("path to string");

    let init = Command::new(env!("CARGO_BIN_EXE_llmenv"))
        .arg("init")
        .arg(config_dir)
        .output()
        .expect("run init command");
    assert!(
        init.status.success(),
        "init should succeed; stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let export = Command::new(env!("CARGO_BIN_EXE_llmenv"))
        .arg("export")
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_EXTRA_TAGS", "extra-tag-probe")
        .output()
        .expect("run export command");
    assert!(
        export.status.success(),
        "export should succeed; stderr: {}",
        String::from_utf8_lossy(&export.stderr)
    );

    let stdout = String::from_utf8_lossy(&export.stdout);
    assert!(
        stdout.contains("extra-tag-probe"),
        "LLMENV_EXTRA_TAGS must appear in export output, got: {stdout}"
    );
}

#[test]
fn export_scope_narrowing_still_includes_extra_tags() {
    // Regression test: `export --scope <id>` rebuilds its tag set from just
    // the matched scope (plus the OS tag) rather than reusing the full
    // active-scope union — extra_tags must be re-added there too, or the
    // narrowed export silently drops tags the unnarrowed export includes.
    let temp = TempDir::new().expect("create temp dir");
    let config_dir = temp.path().to_str().expect("path to string");
    std::fs::write(
        temp.path().join("config.yaml"),
        "scope:\n  content:\n    - id: c\n      match:\n        glob: \"*\"\n      tags: [always]\n",
    )
    .expect("write config.yaml");

    let export = Command::new(env!("CARGO_BIN_EXE_llmenv"))
        .arg("export")
        .arg("--scope")
        .arg("c")
        .current_dir(temp.path())
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_EXTRA_TAGS", "extra-tag-probe")
        .output()
        .expect("run export command");
    assert!(
        export.status.success(),
        "export --scope should succeed; stderr: {}",
        String::from_utf8_lossy(&export.stderr)
    );

    let stdout = String::from_utf8_lossy(&export.stdout);
    assert!(
        stdout.contains("extra-tag-probe"),
        "extra_tags must survive --scope narrowing, got: {stdout}"
    );
}

#[test]
fn export_ignores_non_utf8_llmenv_extra_tags() {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let temp = TempDir::new().expect("create temp dir");
        let config_dir = temp.path().to_str().expect("path to string");

        let init = Command::new(env!("CARGO_BIN_EXE_llmenv"))
            .arg("init")
            .arg(config_dir)
            .output()
            .expect("run init command");
        assert!(init.status.success());

        let non_utf8 = std::ffi::OsStr::from_bytes(b"tag\xff\xfe");
        let export = Command::new(env!("CARGO_BIN_EXE_llmenv"))
            .arg("export")
            .env("LLMENV_CONFIG_DIR", config_dir)
            .env("LLMENV_EXTRA_TAGS", non_utf8)
            .output()
            .expect("run export command");

        // Non-UTF-8 input must degrade gracefully (no extra tags), not crash.
        assert!(
            export.status.success(),
            "export should still succeed on non-UTF-8 $LLMENV_EXTRA_TAGS; stderr: {}",
            String::from_utf8_lossy(&export.stderr)
        );
    }
}
