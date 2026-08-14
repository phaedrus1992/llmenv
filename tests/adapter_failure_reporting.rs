//! #1345/#1346: an engine config llmenv could not render has to be visible.
//!
//! Two separate failures used to hide it. A permission rule with no opencode
//! equivalent was dropped through `tracing::warn!`, which the default
//! `EnvFilter` (`ERROR`) discards, so it reached neither stderr nor the log
//! file. And when an adapter failed outright, `regenerate` printed a warning
//! and still exited 0 as long as another adapter succeeded — reporting success
//! while one engine kept a stale config.
//!
//! These tests pin the observable contract from outside the binary: what lands
//! on stderr, and what the exit code is.
#![expect(clippy::unwrap_used, reason = "test scaffolding")]
use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

mod support;

/// Both adapters must look installed, since the point of #1346 is what happens
/// when one of several adapters fails. `binary_on_path` shells out to `which`,
/// so a stub on `PATH` is enough — neither binary is ever executed.
fn stub_engine_binaries(dir: &Path) -> String {
    let bin = dir.join("stub-bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["claude", "opencode"] {
        let path = bin.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    // `which` itself has to stay reachable.
    format!("{}:/usr/bin:/bin", bin.display())
}

fn current_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}

/// A config whose single bundle carries `permission_rules`, with both engines
/// eligible so one adapter can fail while the other succeeds.
fn setup(permission_rules: &str) -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let cache_dir = dir.path().join("cache");
    let config = format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

bundle:
  - name: test-bundle
    when: [test]

cache:
  cache_dir: "{cache}"
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
        cache = cache_dir.display(),
    );
    fs::write(dir.path().join("config.yaml"), config).unwrap();

    let bundle_dir = dir.path().join("bundles").join("test-bundle");
    fs::create_dir_all(&bundle_dir).unwrap();
    fs::write(bundle_dir.join("bundle.yaml"), permission_rules).unwrap();

    let path = stub_engine_binaries(dir.path());
    (dir, path)
}

fn llmenv(dir: &TempDir, path: &str, subcommand: &str) -> Command {
    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", dir.path().join("config.yaml"))
        .env("PATH", path)
        // Explicitly unset, so the test proves the report survives the default
        // filter rather than a developer's inherited RUST_LOG.
        .env_remove("RUST_LOG")
        .arg(subcommand);
    cmd
}

/// #1345: a rule for a tool opencode has no key for is dropped — say so where
/// the user can see it, with no `RUST_LOG` set.
#[test]
fn dropped_permission_rule_is_reported_on_stderr() {
    let (dir, path) = setup("permissions:\n  deny:\n    - { tool: NotebookEdit }\n");
    llmenv(&dir, &path, "regenerate")
        .assert()
        .stderr(predicate::str::contains("NotebookEdit"))
        .stderr(predicate::str::contains("no permission key"));
}

/// #1345: `Skill` has an exact opencode equivalent, so it must map rather than
/// be reported as unmappable.
#[test]
fn skill_rule_is_not_reported_as_unmappable() {
    let (dir, path) = setup("permissions:\n  deny:\n    - { tool: Skill }\n");
    llmenv(&dir, &path, "regenerate")
        .assert()
        .success()
        .stderr(predicate::str::contains("Regenerated opencode"))
        .stderr(predicate::str::contains("no permission key for neutral tool 'Skill'").not());
}

/// #1346: an unrenderable rule fails the opencode adapter. `regenerate` must
/// exit non-zero and name it, not print a warning above a success message.
#[test]
fn regenerate_fails_and_names_the_adapter_that_failed() {
    // A pattern-scoped rule on an action-only opencode key (#1328).
    let (dir, path) = setup(
        "permissions:\n  allow:\n    - { tool: WebFetch, pattern: \"https://example.com/*\" }\n",
    );
    llmenv(&dir, &path, "regenerate")
        .assert()
        .failure()
        .stderr(predicate::str::contains("adapter regeneration failed for"))
        .stderr(predicate::str::contains("opencode"))
        // The whole reason the loop doesn't abort: every other engine is still
        // regenerated. Without this a future early-return would satisfy the
        // assertions above while breaking the documented contract.
        .stderr(predicate::str::contains("Regenerated claude-code"))
        .stderr(predicate::str::contains("Restart your shell session"));
}

/// #1346: `export` runs on every prompt through the shell hook, so the same
/// failure must *not* fail the command — but it still has to name the adapter
/// whose output is missing.
#[test]
fn export_still_succeeds_but_names_the_failed_adapter() {
    let (dir, path) = setup(
        "permissions:\n  allow:\n    - { tool: WebFetch, pattern: \"https://example.com/*\" }\n",
    );
    llmenv(&dir, &path, "export")
        .assert()
        .success()
        // The surviving adapter's vars are still exported — that is why this
        // path stays exit 0.
        .stdout(predicate::str::contains("export "))
        .stderr(predicate::str::contains(
            "exported environment is missing output from",
        ))
        .stderr(predicate::str::contains("opencode"));
}

/// A config both adapters can render must stay quiet and exit 0 — the checks
/// above must not fire on ordinary configs.
#[test]
fn a_renderable_config_reports_no_failure() {
    let (dir, path) = setup("permissions:\n  allow:\n    - { tool: Bash }\n");
    llmenv(&dir, &path, "regenerate")
        .assert()
        .success()
        // Both engines really rendered — otherwise this control test could pass
        // simply because neither adapter ran.
        .stderr(predicate::str::contains("Regenerated claude-code"))
        .stderr(predicate::str::contains("Regenerated opencode"))
        .stderr(predicate::str::contains("adapter regeneration failed").not());
}
